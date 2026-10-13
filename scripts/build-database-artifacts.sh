#!/usr/bin/env bash
# Populate the shared build/E2E image with the artifacts every integration gate
# consumes. Tests run later from the completed image.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

expected_bazel="$(sed -n '1p' .bazelversion)"
actual_bazel="$(bazel --version)"
if [[ "$actual_bazel" != "bazel $expected_bazel" ]]; then
    printf 'database artifact builder: expected bazel %s, found %s\n' \
        "$expected_bazel" "$actual_bazel" >&2
    exit 1
fi

# Extra flags for the `bazel build` invocations below. The image build sets
# YESNO_BAZEL_DISK_CACHE to a path held in a BuildKit cache mount, so an edit that
# changes nothing Bazel depends on does not recompile Arrow C++, MySQL and
# PostgreSQL from source. It is empty everywhere else, deliberately: a disk cache
# is a build-time optimization and must not change how `bazel` behaves at **run**
# time, where the gates invoke it inside the finished image.
#
# An earlier attempt set the flag through `$HOME/.bazelrc` instead. That file is
# global, so it reached the gate's own in-container `bazel` too, which then failed
# with `/var/cache/yesno-bazel-disk (Permission denied)` because a build-time
# mount no longer exists by then. Scope beats convenience here.
bazel_flags=()
if [[ -n "${YESNO_BAZEL_DISK_CACHE:-}" ]]; then
    bazel_flags+=("--disk_cache=${YESNO_BAZEL_DISK_CACHE}")
fi
# The **repository** cache is a separate thing from the disk cache, and omitting it cost a
# gate run on 2026-10-01: the disk cache holds action outputs, while an external repository's
# download lives in the output base, which an image rebuild discards. So every rebuild
# re-fetched the pinned MySQL tarball from `cdn.mysql.com`, and one fetch failed --
# `Error downloading ... mysql-8.4.0.tar.gz`, with the URL reachable a minute later. Keyed by
# sha256, which is exactly what a pinned download wants, so this makes a rebuild independent
# of upstream availability rather than merely faster.
if [[ -n "${YESNO_BAZEL_REPO_CACHE:-}" ]]; then
    bazel_flags+=("--repository_cache=${YESNO_BAZEL_REPO_CACHE}")
fi

build_postgresql() {
    bazel build "${bazel_flags[@]}" \
        //yesno-pg:yesno_pg \
        //yesno-pg:unit \
        //e2e/postgresql:regress
    bazel build "${bazel_flags[@]}" \
        //yesno-pg:yesno_pg \
        //yesno-pg:unit \
        //e2e/postgresql:regress \
        --//:pg_version=18
}

build_mysql() {
    bazel build "${bazel_flags[@]}" \
        //yesno-c:yesno_c \
        //yesno-flight-c++:unit_tests \
        //third_party/mysql:mysql \
        //e2e/mysql:regress
}

build_search() {
    cargo run --locked -p yesno-e2e --bin yesno-prepare-search
}

# Stage the Java client's live-daemon check so the gate can run it with a plain
# `java -cp`.
#
# Gradle runs **here** and never at test time. The image's Gradle dependency
# cache is a BuildKit cache mount, so it exists while the image is being built
# and is gone by the time a gate runs inside it; anything that invoked Gradle
# from a scenario would reach Maven Central from inside a gate. `liveCheckInstall`
# copies the compiled classes and the whole runtime classpath under
# `yesno-flight-java/build/live-check`, which is an ordinary directory in the
# finished image.
build_java_client() {
    JAVA_HOME="${YESNO_E2E_JAVA_HOME:-${JAVA_HOME:-}}" \
        ./yesno-flight-java/gradlew -p yesno-flight-java --no-daemon liveCheckInstall
}

case "${1:-}" in
    postgresql) build_postgresql ;;
    mysql) build_mysql ;;
    search) build_search ;;
    java) build_java_client ;;
    all)
        build_postgresql
        build_mysql
        build_search
        build_java_client
        ;;
    *)
        printf 'usage: %s {all|postgresql|mysql|search|java}\n' "$0" >&2
        exit 2
        ;;
esac
