# Slim runner for the Terraform-driven real-AWS gates. Unlike the general
# E2E image, it needs no KVM guest, Kubernetes tools, Docker CLI, or fake S3.
#
# One image serves several roles: the harness running a scenario on the EC2
# runner, the failure-path cleanup binary, and -- for the two deferred arms --
# the staging worker, started by ECS inside a Fargate task or by Kubernetes
# inside a Job pod. Those last two are why the `runner` stage has no
# ENTRYPOINT: `yesno-archive` supplies the worker's whole argv as a container
# override or a pod command, and an ENTRYPOINT would prepend the harness to it.
# Every call site names the binary it wants.
#
# The operator arm needs the opposite. A `YesnoCluster` names an image and
# the operator supplies `args` alone, exactly as a user's manifest does, so the
# image it points at must start `yesnod` by itself -- which is what the two
# stages after `runner` are for. They must stay *after* it and `runner` must
# stay the explicit `--target` of the base build: a target-less `docker build`
# takes the last stage, and shipping an ENTRYPOINT image to ECS would prepend
# `yesnod` to the staging worker's argv.
#
# Both are one metadata layer over `runner`, so they add a build of nothing and
# a push of nothing: every layer they need is already in the registry.

ARG RUST_VERSION=1.95
ARG KUBECTL_VERSION=v1.36.1
ARG KUBECTL_SHA256_AMD64=629d3f410e09bf49b64ae7079f7f0bda1191efed311f7d37fdbab0ad5b0ec2b7
ARG KUBECTL_SHA256_ARM64=59f7ee8e477fae658447607dc3c8790ac17a1b016c01c622c12070e969e2d4e7

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /workspace
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --locked --release \
      -p yesno-e2e -p yesno-server -p yesno-server-utils -p yesno-operator && \
    mkdir -p /out && \
    cp target/release/yesno-e2e \
       target/release/yesno-aws-cleanup \
       target/release/yesnod \
       target/release/yesno \
       target/release/yesnoctl \
       target/release/yesno-operator \
       target/release/yesno-snapshot-agent \
       target/release/yesno-snapshot-stage \
       target/release/yesno-archive \
       /out/

# Pinned by digest, like the all-in-one image's copy. The operator arm drives a
# cluster it did not create, so `kubectl` is the harness's only client; the two
# deferred arms reach the same API server with `curl` because their bootstrap
# runs on the host, outside any image.
FROM debian:bookworm-slim AS kubernetes-tools
ARG TARGETARCH
ARG KUBECTL_VERSION
ARG KUBECTL_SHA256_AMD64
ARG KUBECTL_SHA256_ARM64
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    case "${TARGETARCH}" in \
      amd64) kubectl_sum="${KUBECTL_SHA256_AMD64}" ;; \
      arm64) kubectl_sum="${KUBECTL_SHA256_ARM64}" ;; \
      *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    curl --fail --location --output /usr/local/bin/kubectl \
      "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${TARGETARCH}/kubectl" && \
    echo "${kubectl_sum}  /usr/local/bin/kubectl" | sha256sum --check --strict && \
    chmod 0755 /usr/local/bin/kubectl

FROM debian:bookworm-slim AS runner
# The daemon runs unprivileged and the snapshot agent runs as root, so the gate
# proves the privilege split instead of borrowing the container's.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates e2fsprogs libstdc++6 mount util-linux && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --no-create-home --shell /usr/sbin/nologin yesno
COPY --from=build /out/yesno-e2e /out/yesno-aws-cleanup \
    /out/yesnod /out/yesno /out/yesnoctl /out/yesno-operator \
    /out/yesno-snapshot-agent \
    /out/yesno-snapshot-stage /out/yesno-archive /usr/local/bin/
COPY --from=kubernetes-tools /usr/local/bin/kubectl /usr/local/bin/kubectl
COPY e2e/aws/ebs.py /workspace/e2e/aws/ebs.py
COPY e2e/aws/deferred_ecs.py /workspace/e2e/aws/deferred_ecs.py
COPY e2e/aws/deferred_eks.py /workspace/e2e/aws/deferred_eks.py
COPY e2e/aws/operator.py /workspace/e2e/aws/operator.py
# The operator arm installs the checked-in manifests rather than a copy written
# by the harness, so what runs in the cluster is what a user would apply.
COPY yesno-operator/deploy /workspace/yesno-operator/deploy
WORKDIR /workspace

# Named stages, and both empty on purpose. See the header: a `YesnoCluster`
# supplies `args` and no `command`, so the image it names has to start the
# daemon on its own.
FROM runner AS yesnod
ENTRYPOINT ["/usr/local/bin/yesnod"]

FROM runner AS operator
ENTRYPOINT ["/usr/local/bin/yesno-operator"]
