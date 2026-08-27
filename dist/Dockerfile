# Stage two of two: assemble prebuilt binaries into the unified image.
#
#     ./scripts/build-release-image.sh                 # local, host arch
#     ./scripts/build-release-image.sh --tag ghcr.io/moriyoshi/yesnodb:0.1.0 --push
#
# This file compiles nothing. It takes the release binaries from a named
# build context, `binaries`, which must hold one directory per architecture:
#
#     docker buildx build -f dist/Dockerfile \
#         --build-context binaries=target/image-bin ... dist
#
# `dist/build.Dockerfile` is what fills `target/image-bin/<arch>/`, and the
# build script -- or, in CI, a separate per-architecture job -- puts it there.
#
# The main context is `dist/`, not the repository root, and the binaries
# arrive out of band. Two reasons, and both matter: the context stays a few
# kilobytes rather than the whole checkout, so assembly is instantaneous; and
# the compiled output never has to be written inside the version-controlled
# tree to be reachable by a COPY. `target/` is already ignored by git.
#
# One image, one manifest list, `linux/amd64` and `linux/arm64`, carrying every
# shipped binary: `yesnod`, the `yesno` data CLI, `yesnoctl`, `yesno-archive`,
# `yesno-snapshot-stage`, and `yesno-operator`. Which one runs is decided by the
# first argument -- see `dist/entrypoint.sh`, which explains why one image needs
# a dispatcher to satisfy local Docker, ECS, and Kubernetes at once.
#
# ---------------------------------------------------------------------------
# Nothing here executes a foreign instruction, and that is a hard constraint
# ---------------------------------------------------------------------------
#
# Every stage below is either pinned to `$BUILDPLATFORM` or is pure COPY, so
# assembling the whole manifest list needs no QEMU, no `binfmt_misc`, and no
# `tonistiigi/binfmt` install -- on any host, for any architecture. Assembly is
# then a matter of seconds, because all of the cost was paid in stage one.
#
# Do not add a RUN to the `runtime` stage. Anything the runtime needs that is
# architecture-independent ( the CA bundle, `/etc/passwd` ) is prepared in the
# native `rootfs` stage and copied; anything architecture-dependent has to come
# from the base image or be cross-compiled in stage one. A single RUN here
# reintroduces emulation for the whole release, and it will *appear* to work on
# a machine that has binfmt installed.
#
# `yesno-snapshot-agent` is in this image, and it is the one component that
# needs something the base does not have: LVM userspace, for the `lvcreate`,
# `lvremove` and `lvs` it shells out to. ( `mount`, `umount`, `mountpoint` and
# `sync` are already in `debian:*-slim`; only `lvm2` is missing. )
#
# Installing it the ordinary way would need `apt-get` to run as the *target*
# architecture, which is precisely the emulation this file refuses. So the
# `lvmtools` stage below fetches and unpacks the target's `.deb` files
# *natively* instead -- downloading and unpacking are file operations, and
# neither executes target code. See that stage for how the package set is
# chosen.
#
# Shipping the agent's binary is not the same as granting it privilege. This
# image's `USER` is 10001 and stays that way; the agent needs uid 0 and
# CAP_SYS_ADMIN, which its *deployment* grants to that container alone. Do
# not raise this image's USER to 0 to make the agent work -- that would hand the
# daemon the privileges the split exists to deny it.

ARG DEBIAN_SUITE=bookworm

# ---------------------------------------------------------------------------
# rootfs -- architecture-independent pieces of the runtime filesystem.
# ---------------------------------------------------------------------------
#
# Pinned to $BUILDPLATFORM and to the *same suite as the runtime base*, so what
# is copied out is byte-for-byte what an `apt-get install` in the runtime stage
# would have produced -- minus the emulation. Both files below are plain text
# and carry nothing architecture-specific.
#
# The group is created explicitly at gid 10001 rather than left to
# `useradd --user-group`, which allocates from the *system* range and would land
# on 999. `USER` below is numeric `10001:10001` because Kubernetes'
# `runAsNonRoot` can only verify a numeric uid -- a `USER yesno` makes the
# admission check fail closed on an image whose passwd file it cannot read.
FROM --platform=$BUILDPLATFORM debian:${DEBIAN_SUITE}-slim AS rootfs
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 yesno && \
    useradd --uid 10001 --gid 10001 --no-create-home \
        --home-dir /var/lib/yesno --shell /usr/sbin/nologin yesno && \
    install -d -o 10001 -g 10001 -m 0750 /rootfs/var/lib/yesno && \
    install -d -o 0 -g 0 -m 0755 /rootfs/etc/yesno

# ---------------------------------------------------------------------------
# lvmtools -- LVM userspace for the *target* architecture, unpacked natively.
# ---------------------------------------------------------------------------
#
# `yesno-snapshot-agent` shells out to `lvcreate`, `lvremove` and `lvs`, and
# `debian:*-slim` has none of them. Getting them without emulation turns on one
# observation: `apt-get download` and `dpkg-deb -x` only move and unpack files.
# Neither runs any of the target's code, and neither runs a maintainer script.
# So a native stage can enable the foreign architecture, let apt resolve the
# dependency closure, and unpack the result into a directory the runtime copies.
#
# Two filters, and both are load-bearing:
#
#   * **Target architecture only.** Enabling a foreign architecture makes apt
#     willing to satisfy a `Multi-Arch: foreign` dependency with the *native*
#     package, so the download set genuinely does contain a few `_arm64.deb`
#     on an amd64 target. Unpacking one would put host binaries in the image.
#
#   * **Minus what the target base already has**, read out of that image's own
#     `/var/lib/dpkg/status` as data. Without this the closure re-unpacks `libc6`
#     and two dozen more over the base -- larger, and worse than larger: if the
#     archive has moved ahead of the base image's snapshot it would replace libc
#     with a version dpkg does not know it has.
#
# The unpacked tree is rewritten to the merged-`/usr` layout before it is
# copied. Debian is usr-merged but its `.deb` files still ship `/sbin/lvm` and
# friends, and `dpkg` redirects them on install; `dpkg-deb -x` does not, because
# it is only unpacking an archive. Copying the result straight onto the base
# fails with `cannot copy to non-directory: .../lib`, because `/lib`, `/bin` and
# `/sbin` there are symlinks into `/usr`. The error names the destination and
# not the cause, and it appears at the COPY, a stage after the mistake.
#
# `dmsetup` is named explicitly next to `lvm2` even though `lvm2` depends on
# it. It is `Multi-Arch: foreign`, so apt satisfies it with the native package
# and the architecture filter then drops it -- leaving `lvm2` in the image with a
# dependency that is not. Do not trim this list to just `lvm2` on the grounds
# that apt will work the rest out; for foreign-marked packages it works it out
# for the wrong architecture.
FROM debian:${DEBIAN_SUITE}-slim AS runtimebase

FROM --platform=$BUILDPLATFORM debian:${DEBIAN_SUITE}-slim AS lvmtools
ARG TARGETARCH
# Read as data, never executed. This is the target-architecture image, so a
# RUN here would be the emulation this whole file avoids -- but COPY is fine.
COPY --from=runtimebase /var/lib/dpkg/status /tmp/target-dpkg-status
ARG LVM_PACKAGES="lvm2 dmsetup"
RUN set -eu; \
    awk '/^Package: /{p=$2} /^Status: install ok installed/{print p}' \
        /tmp/target-dpkg-status | sort -u > /tmp/base-installed; \
    dpkg --add-architecture "${TARGETARCH}"; \
    apt-get update; \
    mkdir -p /debs /lvmroot; \
    wanted=""; \
    for package in ${LVM_PACKAGES}; do wanted="$wanted ${package}:${TARGETARCH}"; done; \
    apt-get install -y --no-install-recommends --download-only \
        -o Dir::Cache::archives=/debs $wanted; \
    extracted=0; \
    for deb in /debs/*.deb; do \
        case "$deb" in \
            *_"${TARGETARCH}".deb|*_all.deb) ;; \
            *) continue ;; \
        esac; \
        name="$(dpkg-deb -f "$deb" Package)"; \
        if grep -qx "$name" /tmp/base-installed; then continue; fi; \
        echo "unpacking $name"; \
        dpkg-deb -x "$deb" /lvmroot; \
        extracted=$((extracted + 1)); \
    done; \
    rm -rf /debs /var/lib/apt/lists/*; \
    for dir in bin sbin lib lib64; do \
        if [ -d "/lvmroot/$dir" ] && [ ! -L "/lvmroot/$dir" ]; then \
            mkdir -p "/lvmroot/usr/$dir"; \
            cp -a "/lvmroot/$dir/." "/lvmroot/usr/$dir/"; \
            rm -rf "/lvmroot/${dir:?}"; \
        fi; \
    done; \
    [ "$extracted" -gt 0 ]; \
    test -x /lvmroot/usr/sbin/lvm; \
    echo "unpacked $extracted package(s) for ${TARGETARCH}"

# ---------------------------------------------------------------------------
# runtime -- the published image. COPY only; see the header.
# ---------------------------------------------------------------------------
FROM debian:${DEBIAN_SUITE}-slim AS runtime
# Both must be redeclared here to be visible inside the stage. A global `ARG`
# above the first `FROM` reaches `FROM` lines and nothing else, so a stage that
# does not redeclare one expands it to the empty string -- silently. That is not
# hypothetical: `DEBIAN_SUITE` was missing and the base-name label read
# `docker.io/library/debian:-slim`, and `TARGETARCH` missing would have copied
# the entire `binaries` context, every architecture, into `/usr/local/bin/`.
# BuildKit reports it as `UndefinedVar`, which is a warning, not an error.
ARG TARGETARCH
ARG DEBIAN_SUITE

# `ring` links statically, so the runtime needs no crypto package -- only libc,
# which the base image supplies, and the CA bundle, and the latter only because
# a user may point `yesno` at something signed by a public CA.
COPY --from=rootfs /etc/ssl/certs /etc/ssl/certs
COPY --from=rootfs /usr/share/ca-certificates /usr/share/ca-certificates
# Whole-file replacement, not an append, because appending needs a RUN. It is
# exact rather than merely close: the `rootfs` stage is the same image and suite
# as this one, so these are that base's own files plus the `yesno` line.
COPY --from=rootfs /etc/passwd /etc/passwd
COPY --from=rootfs /etc/group /etc/group
COPY --from=rootfs /rootfs/ /

# The one architecture-dependent COPY, and the reason the source path carries
# `${TARGETARCH}`: BuildKit runs this stage once per platform in the manifest
# list, and each pass has to take *that* platform's binaries. A hard-coded path
# here builds a manifest list whose entries all contain the same architecture's
# executables -- which passes every build check and fails at `exec` on the first
# node of the other architecture.
#
# `TARGETARCH` must be redeclared with `ARG` inside this stage to be visible
# here; it is a predefined build argument, not an environment variable, and a
# stage that does not declare it expands `${TARGETARCH}` to the empty string --
# which would copy the whole `binaries` context into `/usr/local/bin/`.
# LVM userspace for `yesno-snapshot-agent`. Unpacked, not installed: the
# image's dpkg database does not list these packages, which is the price of not
# emulating a package manager. Nothing in the image consults it.
COPY --from=lvmtools /lvmroot/ /

COPY --from=binaries --chmod=0755 ${TARGETARCH}/ /usr/local/bin/
COPY --chmod=0755 entrypoint.sh /usr/local/bin/yesno-entrypoint

# Set at build time by `scripts/build-release-image.sh`; see that script for why
# the revision is passed in rather than read from a `.git` the context excludes.
ARG YESNO_VERSION=0.1.0
ARG YESNO_REVISION=unknown
ARG YESNO_CREATED=unknown
LABEL org.opencontainers.image.title="yesnodb" \
      org.opencontainers.image.description="yesnodb daemon, CLIs, archiver, and Kubernetes operator" \
      org.opencontainers.image.version="${YESNO_VERSION}" \
      org.opencontainers.image.revision="${YESNO_REVISION}" \
      org.opencontainers.image.created="${YESNO_CREATED}" \
      org.opencontainers.image.source="https://github.com/moriyoshi/yesnodb" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.base.name="docker.io/library/debian:${DEBIAN_SUITE}-slim"

USER 10001:10001
WORKDIR /var/lib/yesno
VOLUME /var/lib/yesno

# 50051 Flight, 50052 control and replication, 9750 metrics.
EXPOSE 50051 50052 9750

# Local Docker only. ECS reads the task definition's `healthCheck` and
# Kubernetes reads the pod's probes; neither consults this, so a deployment on
# either has to declare its own. `/readyz` on the metrics listener is the one
# that distinguishes "up" from "serving" -- a standby rebuilding its copy is up
# and is not ready.
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s \
  CMD ["/usr/local/bin/yesno", "--endpoint", "http://127.0.0.1:50051", "status"]

# Exec form, and the dispatcher `exec`s in every branch, so PID 1 ends up
# being the selected binary and SIGTERM reaches it directly. Shutdown drains
# readers and takes a final checkpoint; a shell left in the middle would eat the
# signal and turn a clean stop into the 10-second kill.
ENTRYPOINT ["/usr/local/bin/yesno-entrypoint"]
CMD ["--config", "/etc/yesno/yesnod.toml"]
