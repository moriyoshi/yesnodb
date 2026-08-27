package dev.yesnodb.search;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;

import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.net.URI;
import org.apache.arrow.flight.Location;
import org.junit.jupiter.api.Test;
import org.roaringbitmap.RoaringBitmap;
import org.roaringbitmap.longlong.Roaring64NavigableMap;

/** Application-side integration test entered only by the heavyweight search E2E harness. */
final class YesnoSearchApplicationE2ETest {
  @Test
  void resolvesARealFlightResultForBothSearchAdapters() throws Exception {
    URI endpoint = URI.create(System.getProperty("yesno.e2e.flight.location"));
    Location location = Location.forGrpcInsecure(endpoint.getHost(), endpoint.getPort());

    try (YesnoClient client = YesnoClient.connect(location)) {
      YesnoBitmapResolver resolver = new YesnoBitmapResolver(client);
      PreparedBitmap integerBitmap =
          resolver.resolve(
              SetExpression.key(42),
              BitmapWidth.INTEGER_32,
              new SnapshotMode.Current(0));
      assertEquals(3, integerBitmap.cardinality());
      RoaringBitmap integerValues = new RoaringBitmap();
      integerValues.deserialize(java.nio.ByteBuffer.wrap(integerBitmap.portableBytes()));
      assertArrayEquals(new int[] {1, 3, 5}, integerValues.toArray());
      assertEquals(
          "{\"terms\":{\"ordinal\":[\"OjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==\"],"
              + "\"value_type\":\"bitmap\"}}",
          OpenSearchQueryAdapter.queryClauseJson("ordinal", integerBitmap));

      PreparedBitmap longBitmap =
          resolver.resolve(
              SetExpression.key(42), BitmapWidth.LONG_64, new SnapshotMode.Current(0));
      Roaring64NavigableMap longValues = new Roaring64NavigableMap();
      longValues.deserializePortable(
          new DataInputStream(new ByteArrayInputStream(longBitmap.portableBytes())));
      assertArrayEquals(new long[] {1, 3, 5}, longValues.toArray());
      assertEquals(
          "{\"bitmap_terms\":{\"field\":\"ordinal\",\"value\":"
              + "\"AQAAAAAAAAAAAAAAOjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==\"}}",
          ElasticsearchQueryAdapter.queryClauseJson("ordinal", longBitmap));
    }
  }
}
