# yesnodb Arrow Flight client for Java

`yesno-flight-client` is a synchronous Java 17 client for yesnodb's Arrow
Flight v1 service. Its public packages are under `dev.yesnodb.client`.

The client covers:

- key, Boolean, and ordinal-set literal planning with an exact cardinality;
- snapshot-versioned ticket decoding and reusable plan/fetch calls;
- streamed UInt64 ordinal batches, with an explicit materializing helper;
- bounded bulk insert and removal, plus one-batch commit helpers;
- populated-key listing; and
- the `stats` action.

The artifact is not published yet. Build it from this directory with Gradle:

```console
./gradlew build
```

Apache Arrow's Netty memory backend needs this JVM option on Java 17 and later:

```text
--add-opens=java.base/java.nio=ALL-UNNAMED
```

## Query and ingest

```java
import dev.yesnodb.client.OrdinalPair;
import dev.yesnodb.client.QueryStream;
import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import java.util.List;
import org.apache.arrow.flight.Location;

Location location = Location.forGrpcInsecure("127.0.0.1", 50051);
try (YesnoClient client = YesnoClient.connect(location)) {
  client.insert(List.of(new OrdinalPair(42, 10), new OrdinalPair(42, 11)));

  SetExpression expression = SetExpression.and(
      SetExpression.key(42),
      SetExpression.literal(11, 10, 11));

  long exactCount = client.queryCardinality(expression);
  long snapshotVersion;
  try (QueryStream result = client.query(expression)) {
    snapshotVersion = result.info().version();
    List<Long> ordinals = result.collectOrdinals();
    if (ordinals.size() != exactCount) {
      throw new AssertionError("the client also enforces this count");
    }
  }

  // Strictly re-plan the same expression at a retained snapshot version.
  client.prepareQueryAt(expression, snapshotVersion);
}
```

`QueryStream.next()` and `QueryStream.root()` expose batches without
materializing the result. A query ticket is not a lease: if the server has
reclaimed its version, plan the query again before retrying the fetch.

## RoaringBitmap interoperability

Queries whose ordinals fit the unsigned 32-bit domain can be materialized
directly as `org.roaringbitmap.RoaringBitmap` values. The conversion is
loss-checked; it throws rather than truncate if any ordinal exceeds `2^32 - 1`.

```java
RoaringBitmap bitmap = client.getRoaringBitmap(42);
client.insert(43, bitmap);
client.removeBatch(43, RoaringBitmap.bitmapOf(1, 3));
```

This is in-memory interoperability with the Java 32-bit `RoaringBitmap`. The
incompatible serialized `Roaring64NavigableMap` format is deliberately not
accepted; use ordinary 64-bit query streaming for ordinals outside the 32-bit
domain.

## Unsigned 64-bit values

Java has no primitive unsigned 64-bit type. Keys, ordinals, versions, strides,
and counters therefore use the raw bits of a `long`; negative Java values are
valid unsigned values. `UnsignedLongs` converts at decimal-string and
`BigInteger` boundaries:

```java
long maximum = UnsignedLongs.parse("18446744073709551615");
assert maximum == -1L;
assert UnsignedLongs.toString(maximum).equals("18446744073709551615");
```

The maximum unsigned value is a valid key but is reserved as the exclusive
ordinal-universe bound, so it must not be ingested as an ordinal.

## Custom Flight transport

For TLS, client certificates, middleware, or other Flight configuration, build
an Apache Arrow `FlightClient` yourself and wrap it:

```java
try (RootAllocator allocator = new RootAllocator()) {
  FlightClient flight = FlightClient.builder(allocator, location)
      .trustedCertificates(certificateInputStream)
      .build();
  try (YesnoClient client = new YesnoClient(allocator, flight)) {
    // The wrapper owns flight; allocator remains caller-owned.
  }
}
```

Every operation also accepts Arrow `CallOption` values, including header and
timeout options.
