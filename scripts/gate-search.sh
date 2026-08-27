#!/usr/bin/env bash
# Opt-in search scenarios, run by the ordinary yesno-e2e Monty harness inside
# the shared PostgreSQL/MySQL/OpenSearch/Elasticsearch build/E2E image.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

if [[ "${1:-}" != "--container-internal" ]]; then
  exec ./scripts/run-database-gate.sh search "$@"
fi
shift

workspace=$PWD
scenario_dir="$workspace/e2e/search"

case "${1:-}" in
  "")
    scenarios=(
      "$scenario_dir/application.py"
      "$scenario_dir/opensearch.py"
      "$scenario_dir/elasticsearch.py"
    )
    ;;
  --application-only) scenarios=("$scenario_dir/application.py") ;;
  --opensearch-only) scenarios=("$scenario_dir/opensearch.py") ;;
  --elasticsearch-only) scenarios=("$scenario_dir/elasticsearch.py") ;;
  *)
    printf 'usage: %s [--application-only|--opensearch-only|--elasticsearch-only]\n' "$0" >&2
    exit 2
    ;;
esac

exec cargo run --manifest-path "$workspace/Cargo.toml" -p yesno-e2e --bin yesno-e2e -- \
  --timeout 1200 "${scenarios[@]}"
