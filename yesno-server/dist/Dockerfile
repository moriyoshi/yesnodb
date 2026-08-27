# yesnod, clients, archiver, and snapshot materializer, built from the workspace root:
#
#     docker build -f yesno-server/dist/Dockerfile -t yesnod .
#
# **Deploy with `Recreate`, never `RollingUpdate`.** `Db` takes an exclusive
# flock on its directory, so the new pod's `Db::open` fails with "already open"
# while the old one still holds it — and a rolling update is defined by that
# overlap. This is the design ( one process per database ), not a limitation to
# schedule around.
#
# And the volume must be a **local** filesystem. The store is mmap'd, and mmap
# reports I/O failure as SIGBUS, which is not catchable as a `Result` — so a
# network-backed volume turns a transient error into a crash.

ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src

# No `protoc` and no CMake installed, and both absences are deliberate.
# `yesno-server` depends on `protoc-bin-vendored` so that a follower
# operator building in a hurry needs nothing on the host; TLS uses tonic's
# `tls-ring` rather than `tls-aws-lc` for the same reason, because `aws-lc-rs`
# wants CMake and a C toolchain while `ring` needs only `cc`. Adding either
# package here would quietly make that argument untrue for everyone who copies
# this file.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p yesno-server -p yesno-server-utils && \
    mkdir -p /out && cp target/release/yesnod target/release/yesno \
    target/release/yesnoctl target/release/yesno-archive \
    target/release/yesno-snapshot-stage /out/

# `ring` links statically, so the runtime needs no crypto package — only libc
# and the CA bundle, and the latter only because a user may point `yesno` at
# something signed by a public CA.
FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --home /var/lib/yesno --create-home yesno
COPY --from=build /out/yesnod /out/yesno /out/yesnoctl /out/yesno-archive \
    /out/yesno-snapshot-stage \
    /usr/local/bin/

USER yesno
WORKDIR /var/lib/yesno
VOLUME /var/lib/yesno
EXPOSE 50051 50052 9750

# The healthcheck talks to the metrics listener, which by default binds
# loopback — reachable from inside the container, which is where this runs, and
# not from outside, which is the point. `/readyz` distinguishes "up" from
# "serving": a standby rebuilding its copy is up and is not ready.
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s \
  CMD ["/usr/local/bin/yesno", "--endpoint", "http://127.0.0.1:50051", "status"]

# SIGTERM must reach the process for shutdown to drain readers and take its
# final checkpoint, so this is the exec form with no shell in between.
ENTRYPOINT ["/usr/local/bin/yesnod"]
CMD ["--config", "/etc/yesno/yesnod.toml"]
