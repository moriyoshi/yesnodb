# Privileged LVM snapshot sidecar. Build from the workspace root:
#
#   docker build -f yesno-server/dist/snapshot-agent.Dockerfile \
#     -t yesno-snapshot-agent .
#
# Run this image as uid/gid 0 with CAP_SYS_ADMIN. It also needs the host block
# devices and bidirectional mount propagation for the configured source and
# snapshot mount roots. Do not grant those privileges to the yesnod container.

ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p yesno-server --bin yesno-snapshot-agent && \
    cp target/release/yesno-snapshot-agent /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends lvm2 util-linux && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/yesno-snapshot-agent /usr/local/bin/
USER 0:0
ENTRYPOINT ["/usr/local/bin/yesno-snapshot-agent"]
CMD ["--config", "/etc/yesno/yesnod.toml"]
