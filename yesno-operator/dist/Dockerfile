ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p yesno-operator && \
    cp target/release/yesno-operator /usr/local/bin/yesno-operator

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --home /nonexistent --no-create-home yesno
COPY --from=build /usr/local/bin/yesno-operator /usr/local/bin/yesno-operator
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/yesno-operator"]
