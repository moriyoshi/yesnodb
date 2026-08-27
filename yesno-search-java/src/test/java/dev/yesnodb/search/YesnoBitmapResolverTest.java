package dev.yesnodb.search;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import dev.yesnodb.client.QueryTicket;
import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import org.apache.arrow.flight.FlightDescriptor;
import org.apache.arrow.flight.FlightEndpoint;
import org.apache.arrow.flight.FlightInfo;
import org.apache.arrow.flight.FlightProducer;
import org.apache.arrow.flight.FlightServer;
import org.apache.arrow.flight.Location;
import org.apache.arrow.flight.NoOpFlightProducer;
import org.apache.arrow.flight.Ticket;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.memory.RootAllocator;
import org.apache.arrow.vector.UInt8Vector;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.apache.arrow.vector.types.pojo.ArrowType;
import org.apache.arrow.vector.types.pojo.Field;
import org.apache.arrow.vector.types.pojo.FieldType;
import org.apache.arrow.vector.types.pojo.Schema;
import org.junit.jupiter.api.Test;
import org.roaringbitmap.RoaringBitmap;
import org.roaringbitmap.longlong.Roaring64NavigableMap;

class YesnoBitmapResolverTest {
  private static final long VERSION = 9;

  @Test
  // FlightServer.close exposes InterruptedException; the test deliberately owns it here.
  @SuppressWarnings("try")
  void resolvesPortableBitmapsAndNativeQueryClauses() throws Exception {
    try (Fixture fixture = new Fixture(List.of(1L, 3L, 5L), 3)) {
      YesnoBitmapResolver resolver = new YesnoBitmapResolver(fixture.client);

      PreparedBitmap bitmap32 =
          resolver.resolve(
              SetExpression.key(42),
              BitmapWidth.INTEGER_32,
              new SnapshotMode.Current(0),
              ResolveLimits.defaults());
      assertEquals(VERSION, bitmap32.yesnoVersion());
      assertEquals(3, bitmap32.cardinality());
      assertEquals("OjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==", bitmap32.base64());
      RoaringBitmap decoded32 = new RoaringBitmap();
      decoded32.deserialize(ByteBuffer.wrap(bitmap32.portableBytes()));
      assertEquals(RoaringBitmap.bitmapOf(1, 3, 5), decoded32);

      PreparedBitmap bitmap64 =
          resolver.resolve(
              SetExpression.key(42),
              BitmapWidth.LONG_64,
              new SnapshotMode.Pinned(VERSION),
              ResolveLimits.defaults());
      assertEquals("AQAAAAAAAAAAAAAAOjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==", bitmap64.base64());
      Roaring64NavigableMap decoded64 = new Roaring64NavigableMap();
      decoded64.deserializePortable(
          new DataInputStream(new ByteArrayInputStream(bitmap64.portableBytes())));
      assertArrayEquals(new long[] {1, 3, 5}, decoded64.toArray());

      assertEquals(
          "{\"terms\":{\"product_id\":[\"OjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==\"],"
              + "\"value_type\":\"bitmap\"}}",
          OpenSearchQueryAdapter.queryClauseJson("product_id", bitmap32));
      assertEquals(
          "{\"bitmap_terms\":{\"field\":\"product_id\",\"value\":"
              + "\"AQAAAAAAAAAAAAAAOjAAAAEAAAAAAAIAEAAAAAEAAwAFAA==\"}}",
          ElasticsearchQueryAdapter.queryClauseJson("product_id", bitmap64));
    }
  }

  @Test
  @SuppressWarnings("try")
  void enforcesCardinalityBeforeOpeningTheStream() throws Exception {
    try (Fixture fixture = new Fixture(List.of(1L, 2L, 3L), 3)) {
      ResultLimitExceededException error =
          assertThrows(
              ResultLimitExceededException.class,
              () ->
                  new YesnoBitmapResolver(fixture.client)
                      .resolve(
                          SetExpression.key(42),
                          BitmapWidth.INTEGER_32,
                          new SnapshotMode.Current(0),
                          new ResolveLimits(2, 1024)));
      assertEquals(3, error.promised());
      assertEquals(2, error.limit());
      assertEquals(0, fixture.producer.streamCalls.get());
    }
  }

  @Test
  @SuppressWarnings("try")
  void rejectsOutOfDomainAndMalformedResults() throws Exception {
    try (Fixture fixture = new Fixture(List.of((long) Integer.MAX_VALUE + 1), 1)) {
      OrdinalOutOfRangeException error =
          assertThrows(
              OrdinalOutOfRangeException.class,
              () ->
                  new YesnoBitmapResolver(fixture.client)
                      .resolve(
                          SetExpression.key(42),
                          BitmapWidth.INTEGER_32,
                          new SnapshotMode.Current(0)));
      assertEquals(BitmapWidth.INTEGER_32, error.width());
    }

    try (Fixture fixture = new Fixture(List.of(Long.MIN_VALUE), 1)) {
      assertThrows(
          OrdinalOutOfRangeException.class,
          () ->
              new YesnoBitmapResolver(fixture.client)
                  .resolve(
                      SetExpression.key(42),
                      BitmapWidth.LONG_64,
                      new SnapshotMode.Current(0)));
    }

    try (Fixture fixture = new Fixture(List.of(3L, 2L), 2)) {
      assertThrows(
          IllegalStateException.class,
          () ->
              new YesnoBitmapResolver(fixture.client)
                  .resolve(
                      SetExpression.key(42),
                      BitmapWidth.INTEGER_32,
                      new SnapshotMode.Current(0)));
    }

    try (Fixture fixture = new Fixture(List.of(1L), 2)) {
      assertThrows(
          IllegalStateException.class,
          () ->
              new YesnoBitmapResolver(fixture.client)
                  .resolve(
                      SetExpression.key(42),
                      BitmapWidth.INTEGER_32,
                      new SnapshotMode.Current(0)));
    }
  }

  @Test
  void adaptersEscapeFieldNamesAndUseMatchNone() {
    PreparedBitmap empty = new PreparedBitmap(BitmapWidth.INTEGER_32, 4, 0, new byte[0]);
    assertEquals("{\"match_none\":{}}", OpenSearchQueryAdapter.queryClauseJson("ignored", empty));
    assertEquals(
        "{\"match_none\":{}}", ElasticsearchQueryAdapter.queryClauseJson("ignored", empty));

    PreparedBitmap one = new PreparedBitmap(BitmapWidth.INTEGER_32, 4, 1, new byte[] {1});
    assertEquals(
        "{\"terms\":{\"a\\\"\\nb\":[\"AQ==\"],\"value_type\":\"bitmap\"}}",
        OpenSearchQueryAdapter.queryClauseJson("a\"\nb", one));
    assertThrows(
        IllegalArgumentException.class,
        () -> OpenSearchQueryAdapter.queryClauseJson("\ud800", one));
  }

  @Test
  void parsesEngineNeutralExpressionMaps() {
    SetExpression expression =
        SetExpressionMaps.parse(
            Map.of(
                "and_not",
                Map.of(
                    "include",
                    Map.of("or", List.of(Map.of("key", "42"), Map.of("range", Map.of("lo", 5, "hi", 9)))),
                    "exclude",
                    Map.of("empty", Map.of()))));
    assertEquals(
        SetExpression.andNot(
            SetExpression.or(SetExpression.key(42), SetExpression.range(5, 9)),
            SetExpression.empty()),
        expression);
    assertEquals(expression, SetExpressionMaps.parse(SetExpressionMaps.toMap(expression)));
    SetExpression literal = SetExpression.literal(9, Long.MIN_VALUE, 9, -2L);
    assertEquals(literal, SetExpressionMaps.parse(SetExpressionMaps.toMap(literal)));
    assertEquals(
        literal,
        SetExpressionMaps.parse(
            Map.of(
                "literal",
                List.of("9", "9223372036854775808", "9", "18446744073709551614"))));
    assertThrows(
        IllegalArgumentException.class,
        () -> SetExpressionMaps.parse(Map.of("literal", List.of("18446744073709551615"))));

    assertEquals(SetExpression.key(-1L), SetExpressionMaps.parse(Map.of("key", "18446744073709551615")));
    assertThrows(IllegalArgumentException.class, () -> SetExpressionMaps.parse(Map.of("key", -1)));
    assertThrows(
        IllegalArgumentException.class,
        () -> SetExpressionMaps.parse(Map.of("range", Map.of("lo", 1, "extra", 2))));
  }

  @SuppressWarnings("try")
  private static final class Fixture implements AutoCloseable {
    private final RootAllocator allocator = new RootAllocator();
    private final TestProducer producer;
    private final FlightServer server;
    private final YesnoClient client;

    private Fixture(List<Long> ordinals, long promised) throws Exception {
      producer = new TestProducer(allocator, ordinals, promised);
      server =
          FlightServer.builder(
                  allocator, Location.forGrpcInsecure("127.0.0.1", 0), producer)
              .build()
              .start();
      client = YesnoClient.connect(allocator, server.getLocation());
    }

    @Override
    public void close() throws Exception {
      client.close();
      server.close();
      allocator.close();
    }
  }

  private static final class TestProducer extends NoOpFlightProducer {
    private static final Field ORDINAL_FIELD =
        new Field("ordinal", FieldType.notNullable(new ArrowType.Int(64, false)), null);
    private static final Schema ORDINAL_SCHEMA = new Schema(List.of(ORDINAL_FIELD));

    private final BufferAllocator allocator;
    private final List<Long> ordinals;
    private final long promised;
    private final AtomicInteger streamCalls = new AtomicInteger();

    private TestProducer(BufferAllocator allocator, List<Long> ordinals, long promised) {
      this.allocator = allocator;
      this.ordinals = ordinals;
      this.promised = promised;
    }

    @Override
    public FlightInfo getFlightInfo(
        FlightProducer.CallContext context, FlightDescriptor descriptor) {
      return new FlightInfo(
          ORDINAL_SCHEMA,
          descriptor,
          List.of(new FlightEndpoint(new Ticket(ticket(VERSION, 42)))),
          -1,
          promised);
    }

    @Override
    public void getStream(
        FlightProducer.CallContext context,
        Ticket ticket,
        FlightProducer.ServerStreamListener listener) {
      streamCalls.incrementAndGet();
      try (VectorSchemaRoot root =
          VectorSchemaRoot.of(new UInt8Vector(ORDINAL_FIELD, allocator))) {
        UInt8Vector vector = (UInt8Vector) root.getVector("ordinal");
        vector.allocateNew(ordinals.size());
        for (int index = 0; index < ordinals.size(); index++) {
          vector.set(index, ordinals.get(index));
        }
        vector.setValueCount(ordinals.size());
        root.setRowCount(ordinals.size());
        listener.start(root);
        if (!ordinals.isEmpty()) {
          listener.putNext();
        }
        listener.completed();
      }
    }

    private static byte[] ticket(long version, long key) {
      return ByteBuffer.allocate(QueryTicket.HEADER_LENGTH)
          .order(ByteOrder.LITTLE_ENDIAN)
          .putLong(version)
          .putLong(key)
          .putLong(0)
          .putLong(1L << 48)
          .putLong(0)
          .array();
    }
  }
}
