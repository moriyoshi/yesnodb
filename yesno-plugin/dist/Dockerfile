# The example channel peer, as its own image.
#
# Deliberately not part of the unified product image. `yesno-channel-peer` is a
# worked example and an end-to-end fixture, not something a deployment runs, and
# `scripts/check-image-binaries.py` exists precisely so the product image's
# binary set stays the set of binaries the product ships. Adding an example to it
# would be the first exception, and the checker's baseline is empty on purpose.
#
# It is a real image rather than a test harness because the thing being proved --
# that a peer in another container reaches the socket through a shared volume and
# reads a real snapshot -- only exists once there are two containers.
ARG RUST_VERSION=1.98
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p yesno-plugin --bin yesno-channel-peer && \
    cp target/release/yesno-channel-peer /usr/local/bin/yesno-channel-peer

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /nonexistent --no-create-home yesno
COPY --from=build /usr/local/bin/yesno-channel-peer /usr/local/bin/yesno-channel-peer
# 10001 to match `yesnod`: the channel gates on SO_PEERCRED and the socket lands
# at the umask owned by the daemon, so sharing its uid is what makes the socket
# reachable with nothing further configured.
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/yesno-channel-peer"]
