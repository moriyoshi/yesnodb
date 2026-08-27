# yesnodb search adapter for Java

`yesno-search-java` resolves a yesnodb expression through the synchronous Java
Arrow Flight client, validates and bounds the result, and serializes it into the
portable Roaring format accepted by OpenSearch and Elasticsearch bitmap
queries.

The module does not own an OpenSearch or Elasticsearch HTTP client. It returns
a query-clause JSON string so applications can retain their existing transport,
authentication, tracing, and request composition.

Build it from the repository root:

```console
yesno-flight-java/gradlew -p yesno-search-java build
```

Apache Arrow's Netty memory backend needs this JVM option on Java 17 and later:

```text
--add-opens=java.base/java.nio=ALL-UNNAMED
```

## Example

```java
PreparedBitmap bitmap =
    new YesnoBitmapResolver(client)
        .resolve(
            SetExpression.and(SetExpression.key(42), SetExpression.literal(7, 3, 7)),
            BitmapWidth.LONG_64,
            new SnapshotMode.Current(1),
            ResolveLimits.defaults());

String openSearchClause =
    OpenSearchQueryAdapter.queryClauseJson("product_id", bitmap);
String elasticsearchClause =
    ElasticsearchQueryAdapter.queryClauseJson("product_id", bitmap);
```

For a common OpenSearch/Elasticsearch schema, `INTEGER_32` accepts ordinals in
`0..2^31-1` and `LONG_64` accepts ordinals in `0..2^63-1`. Values in yesnodb's
upper unsigned domain are rejected rather than reinterpreted as negative search
field values.
