# The one all-in-one E2E image.
#
# Every gate that needs a container runs in this image: PostgreSQL, MySQL,
# OpenSearch and Elasticsearch through `scripts/run-database-gate.sh`, and the
# Kubernetes operator and native-filesystem scenarios through
# `scripts/gate-operator.sh` and `scripts/gate-filesystems.sh`. It carries the
# pinned build toolchains, the complete Bazel, Cargo and Gradle caches, the
# harness and workload binaries, a stateful Winterbaume S3 endpoint, and a
# bootable Linux guest containing ZFS, Btrfs and LVM.
#
# **One image means one build.** Any gate's first run in a fresh checkout
# pays for every integration's artifacts, not just its own; the trade is that an
# unchanged checkout then runs all six gates against retained layers. The
# separately opt-in real-AWS gate is deliberately **not** part of this image:
# `yesno-operator/e2e/aws.Dockerfile` stays a slim runner because that gate
# pushes its image to a per-run ECR repository and pulls it onto an EC2 host,
# where this image's artifact tree would be paid for in transfer on every run.
#
# Do not reintroduce a per-integration image. The three that preceded this
# one duplicated the source copy, the release-binary build and the pinned
# toolchain three ways, and drifted: they pinned two different Rust versions for
# the same `yesno-e2e` binary.
#
# **The other Dockerfiles in the tree are not harness images and must not be
# folded in here.** `yesno-server/dist/Dockerfile`,
# `yesno-server/dist/snapshot-agent.Dockerfile` and
# `yesno-operator/dist/Dockerfile` are the *shipped product* images; the
# operator scenario builds them from the checkout and loads them into kind, so
# building them is part of what that gate asserts. Absorbing them would delete
# the assertion.

ARG RUST_VERSION=1.97.1

# ---------------------------------------------------------------------------
# Pinned external fixtures. Each is above the source-copy boundary so ordinary
# yesno edits never redownload them.
# ---------------------------------------------------------------------------

FROM eclipse-temurin:21-jdk-jammy AS java

FROM debian:bookworm-slim AS winterbaume
ARG TARGETARCH
ARG WINTERBAUME_VERSION=0.2.6
ARG WINTERBAUME_SHA256_AMD64=3977259621a4b7b23619fa5d03f9755c00ed4794aa51db997369347b2f258ece
ARG WINTERBAUME_SHA256_ARM64=38b10b7ddb178ae4939b83e93ee99d676006ec5a7a08fa04934e5e38724cc5f1
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl xz-utils && \
    rm -rf /var/lib/apt/lists/* && \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-gnu; winterbaume_sum="${WINTERBAUME_SHA256_AMD64}" ;; \
      arm64) target=aarch64-unknown-linux-gnu; winterbaume_sum="${WINTERBAUME_SHA256_ARM64}" ;; \
      *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    bundle="winterbaume-server-${target}" && archive="${bundle}.tar.xz" && \
    curl --fail --location --output "/tmp/${archive}" \
      "https://github.com/moriyoshi/winterbaume/releases/download/winterbaume-server-v${WINTERBAUME_VERSION}/${archive}" && \
    echo "${winterbaume_sum}  /tmp/${archive}" | sha256sum --check --strict && \
    tar -xJf "/tmp/${archive}" -C /usr/local/bin --strip-components=1 \
      "${bundle}/winterbaume-server" && \
    chmod 0755 /usr/local/bin/winterbaume-server

FROM debian:bookworm-slim AS kubernetes-tools
ARG TARGETARCH
ARG KIND_VERSION=v0.33.0
ARG KIND_SHA256_AMD64=aee6151561422756b764a4ae28e7f44cda5af5a9eead3cc9985112b1de8d8e0d
ARG KIND_SHA256_ARM64=20022bee6cfcd5086cb7234d218e3454e6090022f2a8f55d1fa7fcf42c3867a2
ARG KUBECTL_VERSION=v1.36.1
ARG KUBECTL_SHA256_AMD64=629d3f410e09bf49b64ae7079f7f0bda1191efed311f7d37fdbab0ad5b0ec2b7
ARG KUBECTL_SHA256_ARM64=59f7ee8e477fae658447607dc3c8790ac17a1b016c01c622c12070e969e2d4e7
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    case "${TARGETARCH}" in \
      amd64) kind_sum="${KIND_SHA256_AMD64}"; kubectl_sum="${KUBECTL_SHA256_AMD64}" ;; \
      arm64) kind_sum="${KIND_SHA256_ARM64}"; kubectl_sum="${KUBECTL_SHA256_ARM64}" ;; \
      *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    curl --fail --location --output /usr/local/bin/kind \
      "https://kind.sigs.k8s.io/dl/${KIND_VERSION}/kind-linux-${TARGETARCH}" && \
    echo "${kind_sum}  /usr/local/bin/kind" | sha256sum --check --strict && \
    curl --fail --location --output /usr/local/bin/kubectl \
      "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${TARGETARCH}/kubectl" && \
    echo "${kubectl_sum}  /usr/local/bin/kubectl" | sha256sum --check --strict && \
    chmod 0755 /usr/local/bin/kind /usr/local/bin/kubectl

FROM docker:29.2.1-cli@sha256:cab69e2d0a1a2ea9a1ce1060252f439e83483ae41ec09317aecb33b08a0656a5 AS docker-cli

# ---------------------------------------------------------------------------
# The shared toolchain. rules_foreign_cc supplies pinned source archives, but
# their conventional configure/CMake builds still need a complete host
# toolchain; qemu, openssh-client and the KVM guest serve the filesystem
# scenarios. Keep this list here rather than making any gate depend on
# workstation packages.
# ---------------------------------------------------------------------------

FROM rust:${RUST_VERSION}-bookworm AS toolchain

ARG TARGETARCH
ARG BAZEL_VERSION=8.4.1
ARG BAZEL_SHA256_AMD64=3453736dc067a9b923f3066aad378282cd864830d9ad135a45ac9c1ebc47bbae
ARG BAZEL_SHA256_ARM64=3f33cf243fb0500993443d896b41ce08659c23f2a65bd2bbd4314801588f6b2f

RUN case "${TARGETARCH}" in \
      amd64) qemu_package=qemu-system-x86 ;; \
      arm64) qemu_package=qemu-system-arm ;; \
      *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
      autoconf automake bison build-essential bzip2 ca-certificates cmake \
      curl file flex git libtool m4 ninja-build openssh-client patch perl \
      pkg-config python3 qemu-utils "${qemu_package}" rsync unzip util-linux \
      xz-utils zip && \
    rm -rf /var/lib/apt/lists/* && \
    case "${TARGETARCH}" in \
      amd64) bazel_arch=x86_64; bazel_sum="${BAZEL_SHA256_AMD64}" ;; \
      arm64) bazel_arch=arm64; bazel_sum="${BAZEL_SHA256_ARM64}" ;; \
    esac && \
    bazel_name="bazel-${BAZEL_VERSION}-linux-${bazel_arch}" && \
    curl --fail --location --output /usr/local/bin/bazel \
      "https://github.com/bazelbuild/bazel/releases/download/${BAZEL_VERSION}/${bazel_name}" && \
    echo "${bazel_sum}  /usr/local/bin/bazel" | sha256sum --check --strict && \
    chmod 0755 /usr/local/bin/bazel

# `yesno-pg` is outside the cargo workspace and the PostgreSQL gate checks its
# formatting, so rustfmt belongs in the toolchain rather than after the source.
RUN rustup component add rustfmt

RUN useradd --create-home --uid 10001 --shell /bin/bash yesno-builder && \
    install -d -o yesno-builder -g yesno-builder /workspace

COPY --from=java /opt/java/openjdk /opt/java/openjdk

# ---------------------------------------------------------------------------
# The release binaries every later stage consumes. One build, one Rust version:
# the harness, the operator, and the shipped server and utility binaries that
# the KVM guest and the operator scenarios exercise.
# ---------------------------------------------------------------------------

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /workspace
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release --bins -p yesno-e2e -p yesno-operator \
      -p yesno-server -p yesno-server-utils && \
    mkdir -p /out && \
    cp target/release/yesno-e2e target/release/yesno-operator \
       target/release/yesno-aws-cleanup \
       target/release/yesnod target/release/yesno \
       target/release/yesno-archive target/release/yesnoctl \
       target/release/yesno-snapshot-stage \
       target/release/yesno-snapshot-agent /out/

# ---------------------------------------------------------------------------
# The bootable filesystem guest.
# ---------------------------------------------------------------------------

# A build-only SSH identity. It grants access only to the disposable guest
# baked into this test image; it is not a product credential.
FROM ubuntu:24.04 AS guest-key
RUN apt-get update && \
    apt-get install -y --no-install-recommends openssh-client && \
    rm -rf /var/lib/apt/lists/* && \
    ssh-keygen -q -t ed25519 -N '' -f /yesno-e2e-key

# Build a conventional systemd guest root. The database binaries are the exact
# artifacts used by the host-side harness build above.
FROM ubuntu:24.04 AS guest-root
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      btrfs-progs ca-certificates initramfs-tools iproute2 linux-image-virtual lvm2 \
      openssh-server systemd-sysv zfsutils-linux && \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/* && \
    rm -f /etc/machine-id && touch /etc/machine-id && \
    install -d -m 0700 /root/.ssh \
      /etc/systemd/network /etc/ssh/sshd_config.d \
      /etc/systemd/system/multi-user.target.wants \
      /etc/systemd/system/serial-getty@.service.d
COPY --from=guest-key /yesno-e2e-key.pub /root/.ssh/authorized_keys
COPY --from=build /out/yesnod /out/yesno /out/yesno-archive /out/yesnoctl \
    /out/yesno-snapshot-stage /out/yesno-snapshot-agent \
    /usr/local/bin/
COPY yesno-operator/e2e/guest/20-wired.network /etc/systemd/network/
COPY yesno-operator/e2e/guest/root-profile /root/.profile
COPY yesno-operator/e2e/guest/serial-autologin.conf \
    /etc/systemd/system/serial-getty@.service.d/override.conf
COPY yesno-operator/e2e/guest/yesno-e2e-sshd.conf /etc/ssh/sshd_config.d/
COPY yesno-operator/e2e/guest/yesnod.service /etc/systemd/system/
COPY yesno-operator/e2e/guest/yesno-snapshot-agent.service /etc/systemd/system/
COPY yesno-operator/e2e/guest/fstab /etc/fstab
RUN chmod 0600 /root/.ssh/authorized_keys && chmod 0644 /root/.profile && \
    ln -s /usr/lib/systemd/system/systemd-networkd.service \
      /etc/systemd/system/multi-user.target.wants/systemd-networkd.service && \
    ln -s /usr/lib/systemd/system/ssh.service \
      /etc/systemd/system/multi-user.target.wants/ssh.service

# Turn the root tree into a compressed, read-only backing image without mounts
# or privileged Docker build steps. Each scenario creates a qcow2 overlay.
FROM ubuntu:24.04 AS guest-image
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && \
    apt-get install -y --no-install-recommends e2fsprogs qemu-utils && \
    rm -rf /var/lib/apt/lists/*
COPY --from=guest-root / /guest/
RUN kernel="$(find /guest/boot -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -n 1)" && \
    initrd="$(find /guest/boot -maxdepth 1 -type f -name 'initrd.img-*' | sort -V | tail -n 1)" && \
    test -n "$kernel" && test -n "$initrd" && \
    cp "$kernel" /vmlinuz && cp "$initrd" /initrd.img && \
    truncate -s 4G /guest.raw && \
    mkfs.ext4 -q -F -d /guest /guest.raw && \
    qemu-img convert -f raw -O qcow2 -c /guest.raw /guest.qcow2

# ---------------------------------------------------------------------------
# The image every gate runs.
# ---------------------------------------------------------------------------

FROM toolchain AS e2e

COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /usr/local/libexec/docker/cli-plugins/docker-buildx \
    /usr/local/libexec/docker/cli-plugins/docker-buildx
COPY --from=kubernetes-tools /usr/local/bin/kind /usr/local/bin/kubectl /usr/local/bin/
COPY --from=winterbaume /usr/local/bin/winterbaume-server /usr/local/bin/
COPY e2e/entrypoint.sh /usr/local/bin/yesno-e2e-entrypoint
RUN chmod 0755 /usr/local/bin/yesno-e2e-entrypoint

# The harness builds the checked-out workload Dockerfiles. Keeping the source at
# the same path used by the build also keeps CARGO_MANIFEST_DIR valid.
COPY --chown=yesno-builder:yesno-builder . /workspace

COPY --from=build /out/yesno-e2e /out/yesno-operator \
    /out/yesno-aws-cleanup /usr/local/bin/
COPY --from=guest-image /guest.qcow2 /vmlinuz /opt/yesno-e2e/
COPY --from=guest-key /yesno-e2e-key /opt/yesno-e2e/ssh-key
RUN chmod 0600 /opt/yesno-e2e/ssh-key

ENV HOME=/home/yesno-builder
ENV JAVA_HOME=/opt/java/openjdk
ENV YESNO_E2E_JAVA_HOME=/opt/java/openjdk
ENV PATH="/opt/java/openjdk/bin:${PATH}"
ENV DOCKER_HOST=unix:///var/run/docker.sock
WORKDIR /workspace
USER yesno-builder

RUN ./scripts/build-database-artifacts.sh postgresql
RUN ./scripts/build-database-artifacts.sh mysql
RUN ./scripts/build-database-artifacts.sh search

# One harness entrypoint. The operator binary and the KVM guest are workload
# artifacts, not additional scenario endpoints, so neither is ever the command.
ENTRYPOINT ["/usr/local/bin/yesno-e2e-entrypoint"]
CMD ["/usr/local/bin/yesno-e2e", "--timeout", "1200", "e2e/operator/operator.py"]
