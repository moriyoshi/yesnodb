# yesnodb OpenSearch plugin

This version-locked OpenSearch 3.8.0 plugin adds a `yesno` query. The
coordinator resolves its Boolean expression through Arrow Flight on the bounded
`yesno_resolve` executor, enforces node-level result limits, and rewrites the
request to the native bitmap-valued `terms` query before shard fan-out.

```json
{
  "query": {
    "yesno": {
      "field": "product_id",
      "width": "long",
      "snapshot": "current",
      "expression": {
        "and_not": {
          "include": { "or": [{ "key": "42" }, { "literal": ["3", "7"] }] },
          "exclude": { "range": { "lo": "1000", "hi": "2000" } }
        }
      }
    }
  }
}
```

Node settings are `yesno.flight.location` (default
`grpc+tcp://127.0.0.1:50051`), `yesno.flight.timeout`, `yesno.max_matches`,
`yesno.max_bitmap_bytes`, and `yesno.current_retries`. A pinned snapshot is
written as `"snapshot": {"version": "123"}` and never falls forward.

Build the installable ZIP with:

```console
yesno-flight-java/gradlew -p yesno-opensearch-plugin clean build
```

OpenSearch must start with
`--add-opens=java.base/java.nio=ALL-UNNAMED` for the Arrow allocator.

The normal ZIP contains no socket grant. The search E2E gate builds a
`-PyesnoE2ePermissions=true` artifact whose policy is limited to outbound
`127.0.0.1` connections and the thread management required by shaded
gRPC/Netty. It grants no inbound socket, filesystem, or process access.
