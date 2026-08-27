# yesnodb Elasticsearch plugin

This version-locked Elasticsearch 9.5.2 plugin adds a `yesno` query. The
coordinator resolves its Boolean expression through Arrow Flight on the bounded
`yesno_resolve` executor, enforces node-level result limits, and transports the
portable bitmap to the shards for constant-score matching against numeric doc
values. This fallback is included because Elasticsearch 9.5.2 does not yet
register the documented `bitmap_terms` query.

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
The target field must be an `integer` or `long` field with `doc_values`
enabled (the mapping default).

Build the installable ZIP with:

```console
yesno-flight-java/gradlew -p yesno-elasticsearch-plugin clean build
```

Elasticsearch must start with
`--add-opens=java.base/java.nio=ALL-UNNAMED` for the Arrow allocator.

The normal ZIP contains no entitlements. The search E2E gate builds a
`-PyesnoE2ePermissions=true` artifact with `outbound_network` and
`manage_threads` for the unnamed shaded gRPC/Netty module. That entitlement
file exists only in the disposable E2E artifact and grants no inbound socket,
filesystem, native-loading, or process access.
