#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

set -euo pipefail

runtime_dir="${TEST_TMPDIR:?}/yesno-flight-cpp-runtime"
mkdir -p "$runtime_dir"

arrow=""
arrow_flight=""
while IFS= read -r library; do
    case "$(basename "$library")" in
        libarrow.so) arrow="$library" ;;
        libarrow_flight.so) arrow_flight="$library" ;;
    esac
done < <(find -L "${TEST_SRCDIR:?}" -type f \
    \( -name libarrow.so -o -name libarrow_flight.so \))

if [[ -z "$arrow" || -z "$arrow_flight" ]]; then
    echo "unit test runfiles do not contain Arrow shared libraries" >&2
    exit 1
fi

# rules_foreign_cc exposes regular linker files while Arrow records versioned
# SONAMEs. Give the loader those names in test-only scratch space.
ln -s "$arrow" "$runtime_dir/libarrow.so.2500"
ln -s "$arrow_flight" "$runtime_dir/libarrow_flight.so.2500"
export LD_LIBRARY_PATH="$runtime_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

exec "$TEST_SRCDIR/_main/yesno-flight-c++/unit_test_bin"
