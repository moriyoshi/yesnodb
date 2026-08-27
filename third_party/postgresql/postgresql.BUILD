"""Overlay applied to the extracted PostgreSQL source tarball.

Built from source rather than found on the host because a `cdylib` PostgreSQL
`dlopen`s must be ABI-identical to the server: same major version, same
`BLCKSZ`, same configure flags. A host package can differ in any of those
without saying so, and the failure mode is a crash inside the backend rather
than a link error.

The configure flags below are part of the ABI, not a convenience. Changing
one — `--with-blocksize` above all — changes `MaxOffsetNumber` and
`MaxHeapTuplesPerPage`, which the index and table access methods compute their
TID packings from. Do not edit them to work around a build failure on a new
host without re-deriving those constants.
"""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "configure_make")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "all_srcs",
    srcs = glob(
        ["**"],
        exclude = ["bazel-*/**"],
    ),
)

configure_make(
    name = "postgresql",
    # A literal, not `-j$(nproc)`. rules_foreign_cc runs `args` through its
    # own shell-fragment rewriter, which parses `$$` as a variable reference and
    # fails at *analysis* time with "Variable or function name is not marked
    # correctly in fragment". There is no escape that survives it, so the
    # parallelism is fixed here and is the one number in this file that is a
    # tuning knob rather than part of the ABI.
    args = ["-j8"],
    configure_in_place = True,
    configure_options = [
        # Every optional dependency is off. The extension needs a server and a
        # client to test against, not a feature-complete distribution, and each
        # of these would be one more thing that must exist on the build host —
        # which is the property this whole target exists to remove.
        "--without-readline",
        "--without-zlib",
        "--without-icu",
        "--without-libxml",
        "--without-openssl",
        "--without-perl",
        "--without-python",
        "--without-tcl",
        # Assertions off: this is the server the regression harness runs
        # against, and an assertion build changes timing enough to make a
        # flake-hunt harder, not easier.
        "--disable-debug",
    ],
    lib_source = ":all_srcs",
    out_binaries = [
        "pg_config",
        "postgres",
        "initdb",
        "pg_ctl",
        "psql",
    ],
    out_data_dirs = ["share"],
    out_static_libs = ["libpgcommon.a", "libpgport.a"],
    targets = [
        "",
        "install",
    ],
)

# rules_foreign_cc publishes each declared output as its own output group. These
# filegroups are what the rest of the build consumes, so nothing downstream has
# to know that PostgreSQL was built by `configure_make` rather than found.
#
# `gen_dir` is the whole install prefix. `initdb` needs `share/` beside the
# binary to find `postgres.bki` and the sample configs, so the regression
# harness depends on the prefix rather than on the individual binaries.

filegroup(
    name = "gen_dir",
    srcs = [":postgresql"],
    output_group = "gen_dir",
)

filegroup(
    name = "pg_config",
    srcs = [":postgresql"],
    output_group = "pg_config",
)

filegroup(
    name = "initdb",
    srcs = [":postgresql"],
    output_group = "initdb",
)

filegroup(
    name = "pg_ctl",
    srcs = [":postgresql"],
    output_group = "pg_ctl",
)

filegroup(
    name = "postgres_bin",
    srcs = [":postgresql"],
    output_group = "postgres",
)

filegroup(
    name = "psql",
    srcs = [":postgresql"],
    output_group = "psql",
)
