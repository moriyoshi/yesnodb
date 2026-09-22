# Stage one of two: cross-compile the release binaries for one architecture.
#
#     docker buildx build -f dist/build.Dockerfile --platform linux/amd64 \
#         --target export --output type=local,dest=dist/bin/amd64 .
#
# This produces *executables*, not an image. `dist/Dockerfile` is stage two and
# assembles them into the published multi-architecture image; it compiles
# nothing. `scripts/build-release-image.sh` drives both.
#
# ---------------------------------------------------------------------------
# Why the two stages are separate artifacts rather than one Dockerfile
# ---------------------------------------------------------------------------
#
# Splitting them is what lets the architectures be built by *separate CI jobs*,
# in parallel, and assembled afterwards. Compilation is the entire cost here --
# minutes per architecture against seconds for the image -- so building them
# concurrently roughly halves the wall clock, and a re-tag or a base-image bump
# re-runs only the cheap half.
#
# ---------------------------------------------------------------------------
# Cross-compilation, never emulation
# ---------------------------------------------------------------------------
#
# This stage is pinned to `$BUILDPLATFORM`: it always runs as native code, on
# whatever machine invokes it, and targets the other architecture through a
# cross toolchain. QEMU is never involved. Emulating a Rust build of this
# dependency tree is not slightly slower, it is roughly an order of magnitude
# slower, and it also fails outright on a host whose `binfmt_misc` entry lacks
# the `F` flag -- the default on an ordinary developer machine, where the
# symptom is the uninformative `exec /usr/bin/...: no such file or directory`.
#
# Do not add `--platform=$TARGETPLATFORM` to this stage, and do not
# "simplify" it by letting BuildKit pick the platform. That silently reintroduces
# emulation and the build still passes, only far slower.

ARG RUST_VERSION=1.95

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS build
ARG TARGETARCH
WORKDIR /src

# The toolchain install is its own layer so that editing source does not
# reinstall it.
#
# Three names per architecture, and none is derivable from the others:
#
#   * the Rust target triple ( `x86_64-unknown-linux-gnu` );
#   * the binutils/gcc tool prefix ( `x86_64-linux-gnu-`, with an underscore );
#   * the Debian package ( `gcc-x86-64-linux-gnu`, with hyphens ).
#
# `gcc-${prefix}` therefore names a package that does not exist, and apt reports
# only `Unable to locate package`, which reads like a missing repository rather
# than a mangled name. The aarch64 spellings happen to coincide, so the mistake
# is invisible until the first cross build to amd64.
#
# `libc6-dev-${TARGETARCH}-cross` is required and is easy to omit: the gcc
# cross package contains a compiler but no target C library headers, so the
# compiler falls back to the *host's* `/usr/include` and fails deep inside
# `ring`'s build script with `bits/libc-header-start.h: No such file or
# directory` -- an error that names neither cross-compilation nor the missing
# package.
RUN set -eux; \
    case "${TARGETARCH}" in \
      amd64) triple=x86_64-unknown-linux-gnu; \
             prefix=x86_64-linux-gnu; package=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; \
             prefix=aarch64-linux-gnu; package=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    printf 'TRIPLE=%s\nPREFIX=%s\n' "$triple" "$prefix" > /etc/yesno-target; \
    rustup target add "$triple"; \
    if ! command -v "${prefix}-gcc" >/dev/null 2>&1; then \
      apt-get update; \
      apt-get install -y --no-install-recommends \
        "$package" "libc6-dev-${TARGETARCH}-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    command -v "${prefix}-gcc"

COPY . .

# No `protoc` and no CMake installed, and both absences are deliberate.
# `yesno-server` depends on `protoc-bin-vendored` so that a follower operator
# building in a hurry needs nothing on the host; TLS uses tonic's `tls-ring`
# rather than `tls-aws-lc` for the same reason, because `aws-lc-rs` wants CMake
# and a C toolchain while `ring` needs only `cc`. Adding either package here
# would quietly make that argument untrue for everyone who copies this file.
# The cross gcc above is not a counterexample: it is the linker for a target the
# host is not, not a build system pulled in to satisfy a dependency.
#
# `protoc-bin-vendored` supplies a *host* binary and the build scripts that
# run it are host code, which is another thing that only works because this
# stage is native. Under emulation it would be the target's protoc.
#
# The binaries are not stripped, deliberately. The release profile is the
# default one, so there is no DWARF to remove and the only thing `strip` would
# take is `.symtab` -- which is exactly what turns a panic backtrace from named
# frames into hex addresses. A few MiB of symbol table is a cheap price for a
# readable crash in a database.
#
# The cache mounts are keyed by architecture and locked. Two architectures
# may be in flight against one BuildKit; an unkeyed, `sharing=shared` `target/`
# would let two cargo invocations interleave fingerprint writes. `--locked` is
# likewise not decoration: a published image must resolve the versions
# `Cargo.lock` records, not whatever is newest on the day it is built.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=yesno-cargo-registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=yesno-target-${TARGETARCH},sharing=locked \
    set -eux; \
    . /etc/yesno-target; \
    linker_var="CARGO_TARGET_$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')_LINKER"; \
    export "${linker_var}=${PREFIX}-gcc"; \
    export "CC_$(echo "$TRIPLE" | tr '-' '_')=${PREFIX}-gcc"; \
    export "AR_$(echo "$TRIPLE" | tr '-' '_')=${PREFIX}-ar"; \
    cargo build --release --locked --target "$TRIPLE" \
        -p yesno-server -p yesno-server-utils -p yesno-operator; \
    mkdir -p /out; \
    for binary in yesnod yesno yesnoctl yesno-archive yesno-snapshot-stage yesno-snapshot-agent yesno-operator; do \
        cp "target/${TRIPLE}/release/${binary}" /out/; \
    done; \
    chmod 0755 /out/*; \
    file /out/yesnod || true

# The export surface. `FROM scratch` so that `--output type=local` writes the
# binaries and nothing else -- a Debian base here would export a whole root
# filesystem alongside them.
FROM scratch AS export
COPY --from=build /out/ /
