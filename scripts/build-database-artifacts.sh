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

build_postgresql() {
    bazel build \
        //yesno-pg:yesno_pg \
        //yesno-pg:unit \
        //e2e/postgresql:regress
    bazel build \
        //yesno-pg:yesno_pg \
        //yesno-pg:unit \
        //e2e/postgresql:regress \
        --//:pg_version=18
}

build_mysql() {
    bazel build \
        //yesno-c:yesno_c \
        //yesno-flight-c++:unit_tests \
        //third_party/mysql:mysql \
        //e2e/mysql:regress
}

build_search() {
    cargo run --locked -p yesno-e2e --bin yesno-prepare-search
}

case "${1:-}" in
    postgresql) build_postgresql ;;
    mysql) build_mysql ;;
    search) build_search ;;
    all)
        build_postgresql
        build_mysql
        build_search
        ;;
    *)
        printf 'usage: %s {all|postgresql|mysql|search}\n' "$0" >&2
        exit 2
        ;;
esac
