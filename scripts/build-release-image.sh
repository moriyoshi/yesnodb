#!/usr/bin/env bash
#
# Build -- and, only when asked, publish -- the unified yesnodb image.
#
#     ./scripts/build-release-image.sh
#         Host architecture only, loaded into the local Docker daemon as
#         `yesnodb:local`. The local-Docker path, and the one to use while
#         iterating; it is a single image rather than a manifest list because
#         `--load` cannot accept one without the containerd image store.
#
#     ./scripts/build-release-image.sh --tag ghcr.io/moriyoshi/yesnodb:0.1.0 --push
#         linux/amd64 + linux/arm64, published as one manifest list. This is
#         what ECS and Kubernetes consume: both resolve the tag against the node
#         architecture, so one reference serves Graviton and x86 nodes alike.
#
#     ./scripts/build-release-image.sh --binaries-only --platform linux/arm64
#     ./scripts/build-release-image.sh --assemble-only --tag ... --push
#         The two halves, run separately. This is how CI uses it: one job per
#         architecture compiles and uploads `target/image-bin/<arch>/`, and a
#         later job downloads them all and assembles the manifest list.
#
# Build output goes to stderr and stdout carries only image references, one per
# line, so a caller can write `image="$(./scripts/build-release-image.sh)"` --
# the same contract as `scripts/build-e2e-image.sh`. With `--tag` repeated
# there is a line per tag and the first is the primary; a single-tag caller is
# unaffected.
#
# ---------------------------------------------------------------------------
# Two stages, because compilation is the whole cost
# ---------------------------------------------------------------------------
#
# `dist/build.Dockerfile` cross-compiles one architecture's binaries into
# `target/image-bin/<arch>/`; `dist/Dockerfile` assembles them and compiles
# nothing. Splitting them lets the architectures be built concurrently -- by
# separate CI jobs -- and makes a re-tag or a base-image bump re-run only the
# cheap half.
#
# Neither half ever emulates. The compile stage is pinned to $BUILDPLATFORM
# and cross-compiles; the assembly stage is pure COPY. So no QEMU, no
# `binfmt_misc`, and no `tonistiigi/binfmt` -- which matters beyond speed,
# because emulation fails outright on a host whose binfmt entry lacks the `F`
# flag, the default on an ordinary developer machine.
#
# ---------------------------------------------------------------------------
# `--push` is the only flag that leaves this machine
# ---------------------------------------------------------------------------
#
# A push is not undoable the way a local build is: a tag may be consumed
# ( and cached, and mirrored ) by anything watching the registry within seconds,
# and deleting it afterwards does not recall it. So publishing is never the
# default and never implied by any other flag.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

DEFAULT_PLATFORMS="linux/amd64,linux/arm64"

# A `docker-container` builder is required for a manifest list; the default
# `docker` driver cannot produce one. Named rather than anonymous so repeated
# runs reuse its cache.
BUILDER="${YESNO_BUILDER:-yesno-multiarch}"

# Under `target/`, which is already ignored by git and by `.dockerignore`, so
# compiled output never lands in the version-controlled tree and never enters
# the compile stage's own build context.
BIN_DIR="${YESNO_IMAGE_BIN_DIR:-target/image-bin}"

# An array: `--tag` may be repeated, which is how a release publishes
# `1.2.3`, `1.2`, `1` and `latest` as aliases of one manifest list in a single
# push rather than four builds.
images=()
platforms=""
push=0
attest=1
do_binaries=1
do_assemble=1

usage() {
    sed -n '2,50p' "$0" | sed 's/^#\{1,2\} \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) images+=("$2"); shift 2 ;;
        --tag=*) images+=("${1#--tag=}"); shift ;;
        --platform) platforms="$2"; shift 2 ;;
        --platform=*) platforms="${1#--platform=}"; shift ;;
        --push) push=1; shift ;;
        --binaries-only) do_assemble=0; shift ;;
        --assemble-only) do_binaries=0; shift ;;
        # Provenance and SBOM attestations add `unknown/unknown` entries to
        # the manifest list. Every current registry and runtime handles them,
        # but a strict downstream mirror or an older replication rule may not,
        # and this is the escape hatch for that rather than a reason to publish
        # unattested by default.
        --no-attest) attest=0; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
done

if [ -z "$platforms" ]; then
    if [ "$push" -eq 1 ] || [ "$do_binaries" -eq 0 ] || [ "$do_assemble" -eq 0 ]; then
        # Anything but the plain local build wants the full set: a publish
        # obviously does, and either half run on its own is the CI shape, where
        # the other half is expecting both architectures to exist.
        platforms="$DEFAULT_PLATFORMS"
    else
        # `--load` takes exactly one platform. Ask Docker which one this host is
        # rather than mapping `uname -m` by hand.
        platforms="linux/$(docker version --format '{{.Server.Arch}}')"
    fi
fi

if [ ${#images[@]} -eq 0 ]; then
    images=("${YESNO_IMAGE:-yesnodb:local}")
fi

docker info >/dev/null

ensure_builder() {
    if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
        echo "==> creating buildx builder \`$BUILDER\` (docker-container driver)" >&2
        docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
    fi
}

# ---------------------------------------------------------------------------
# Stage one: compile, one architecture at a time.
# ---------------------------------------------------------------------------
#
# One `docker buildx build` per platform rather than a single multi-platform
# invocation. A multi-platform `--output type=local` writes `<dest>/<os>_<arch>/`
# subdirectories, whose naming is BuildKit's to change; asking for one
# architecture at a time puts the layout under this script's control, which is
# what lets a CI job build exactly one and upload it.
if [ "$do_binaries" -eq 1 ]; then
    ensure_builder
    IFS=','
    for platform in $platforms; do
        unset IFS
        arch="${platform#*/}"
        echo "==> compiling $platform -> $BIN_DIR/$arch" >&2
        rm -rf "${BIN_DIR:?}/$arch"
        mkdir -p "$BIN_DIR/$arch"
        # Layer cache only, and only when asked for. BuildKit `type=cache`
        # mounts -- which is where the cargo registry and `target/` live -- are
        # never exported by `--cache-to`, so this does not make a changed source
        # tree compile faster. What it does is make an *unchanged* context free,
        # which is the common CI case for a commit that touched only files
        # `.dockerignore` excludes.
        cache_args=()
        if [ -n "${YESNO_BUILDX_CACHE:-}" ]; then
            scope="${YESNO_BUILDX_CACHE_SCOPE:-yesno-$arch}"
            cache_args=(
                --cache-from "type=${YESNO_BUILDX_CACHE},scope=${scope}"
                --cache-to "type=${YESNO_BUILDX_CACHE},scope=${scope},mode=max"
            )
        fi
        docker buildx build \
            --builder "$BUILDER" \
            --file dist/build.Dockerfile \
            --platform "$platform" \
            --target export \
            --output "type=local,dest=$BIN_DIR/$arch" \
            "${cache_args[@]}" \
            --progress=plain \
            . >&2
        IFS=','
    done
    unset IFS
fi

if [ "$do_assemble" -eq 0 ]; then
    printf '%s\n' "$BIN_DIR"
    exit 0
fi

# ---------------------------------------------------------------------------
# Stage two: assemble.
# ---------------------------------------------------------------------------

# Refuse to assemble from binaries that are absent. Without this the COPY
# would fail with a BuildKit path error naming a temporary directory, or --
# worse, if only one architecture were present -- succeed for that one and
# produce a manifest list quietly missing the other.
IFS=','
for platform in $platforms; do
    unset IFS
    arch="${platform#*/}"
    # `-f`, not `-x`. `actions/upload-artifact` does not preserve the
    # executable bit, so a binary that has been through an artifact round trip
    # arrives as mode 0644 and an `-x` test here would reject a perfectly good
    # download. The image is unaffected because the assembly COPY sets the mode
    # explicitly with `--chmod=0755` rather than inheriting it.
    if [ ! -f "$BIN_DIR/$arch/yesnod" ]; then
        echo "no binaries for $platform: \`$BIN_DIR/$arch/yesnod\` is missing." >&2
        echo "Run without --assemble-only, or build that architecture first with" >&2
        echo "  $0 --binaries-only --platform $platform" >&2
        exit 1
    fi
    IFS=','
done
unset IFS

# The revision is passed in because `.dockerignore` excludes `.git` from the
# build context -- deliberately, since a doc-only commit must not invalidate a
# build layer. `--dirty` is not cosmetic: an image built from uncommitted work
# must not claim to be the commit it was built near.
revision="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
if ! git diff --quiet HEAD -- 2>/dev/null; then
    revision="${revision}-dirty"
fi
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
created="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

build_args=(
    --file dist/Dockerfile
    --build-context "binaries=$BIN_DIR"
    --platform "$platforms"
    --build-arg "YESNO_VERSION=${version:-0.0.0}"
    --build-arg "YESNO_REVISION=$revision"
    --build-arg "YESNO_CREATED=$created"
    --progress=plain
)
for tag in "${images[@]}"; do
    build_args+=(--tag "$tag")
done

if [ "$push" -eq 1 ]; then
    for tag in "${images[@]}"; do
        case "$tag" in
            */*) ;;
            *)
                echo "refusing to push \`$tag\`: it names no registry, so it would" >&2
                echo "resolve to Docker Hub's library namespace. Pass a full reference." >&2
                exit 1
                ;;
        esac
    done

    ensure_builder
    build_args+=(--builder "$BUILDER" --push)
    if [ "$attest" -eq 1 ]; then
        build_args+=(--provenance=mode=max --sbom=true)
    fi

    echo "==> publishing" >&2
    printf '      %s\n' "${images[@]}" >&2
    echo "    platforms: $platforms" >&2
    echo "    revision:  $revision" >&2
else
    case "$platforms" in
        *,*)
            # `--load` cannot take a manifest list. Building it without an
            # exporter still proves it assembles, which is what a pull request
            # needs; a caller that wants it locally must name one platform.
            ensure_builder
            build_args+=(--builder "$BUILDER")
            ;;
        *)
            build_args+=(--load)
            ;;
    esac
fi

# The main context is `dist/`, a few kilobytes. The binaries arrive through
# `--build-context` above instead of being copied into the tracked tree.
docker buildx build "${build_args[@]}" dist >&2

printf '%s\n' "${images[@]}"
