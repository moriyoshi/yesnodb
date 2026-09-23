package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.NavigableSet;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentSkipListSet;
import org.apache.arrow.flight.Action;
import org.apache.arrow.flight.Criteria;
import org.apache.arrow.flight.FlightDescriptor;
import org.apache.arrow.flight.FlightEndpoint;
import org.apache.arrow.flight.FlightInfo;
import org.apache.arrow.flight.FlightProducer;
import org.apache.arrow.flight.FlightServer;
import org.apache.arrow.flight.FlightStream;
import org.apache.arrow.flight.Location;
import org.apache.arrow.flight.NoOpFlightProducer;
import org.apache.arrow.flight.PutResult;
import org.apache.arrow.flight.Result;
import org.apache.arrow.flight.Ticket;
import org.apache.arrow.memory.ArrowBuf;
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

class YesnoClientTest {
  @Test
  // FlightServer.close exposes InterruptedException; the test deliberately owns it here.
  @SuppressWarnings("try")
  void clientCoversPlanningFetchingMutationsAndActions() throws Exception {
    try (RootAllocator allocator = new RootAllocator()) {
      TestProducer producer = new TestProducer(allocator);
      producer.values.computeIfAbsent(42L, ignored -> newSet()).addAll(List.of(1L, 3L));

      Location listen = Location.forGrpcInsecure("127.0.0.1", 0);
      try (FlightServer server = FlightServer.builder(allocator, listen, producer).build().start();
          YesnoClient client = YesnoClient.connect(allocator, server.getLocation())) {
        assertEquals(List.of(42L), client.keys());
        assertEquals(2, client.cardinality(42));

        QueryInfo planned = client.prepareKey(42);
        assertEquals(2, planned.totalRecords());
        assertEquals(9, planned.version());
        assertEquals(42, planned.ticket().key());

        try (QueryStream query = client.get(42)) {
          assertEquals(List.of(1L, 3L), query.collectOrdinals());
        }

        SetExpression expression =
            SetExpression.and(SetExpression.key(42), SetExpression.range(0, 10));
        assertEquals(2, client.queryCardinality(expression));
        assertEquals(expression, producer.lastExpression);
        assertEquals(2, client.prepareQueryAt(expression, 9).totalRecords());
        assertEquals(QueryRequest.at(expression, 9), producer.lastRequest);

        SetExpression literal =
            SetExpression.and(SetExpression.key(42), SetExpression.literal(3, 1, 3));
        assertEquals(2, client.queryCardinality(literal));
        assertEquals(literal, producer.lastExpression);

        assertEquals(
            2, client.insert(List.of(new OrdinalPair(42, 5), new OrdinalPair(7, Long.MIN_VALUE))));
        assertEquals(1, client.removeBatch(List.of(new OrdinalPair(42, 1))));
        try (QueryStream query = client.get(42)) {
          assertEquals(List.of(3L, 5L), query.collectOrdinals());
        }
        assertEquals(List.of(7L, 42L), client.keys());

        RoaringBitmap bitmap = RoaringBitmap.bitmapOf(0, Integer.MIN_VALUE, -1);
        assertEquals(3, client.insert(8, bitmap));
        assertEquals(bitmap, client.getRoaringBitmap(8));
        assertEquals(1, client.removeBatch(8, RoaringBitmap.bitmapOf(-1)));
        assertEquals(RoaringBitmap.bitmapOf(0, Integer.MIN_VALUE), client.getRoaringBitmap(8));
        assertEquals(RoaringBitmap.bitmapOf(3, 5), client.queryRoaringBitmap(expression));

        client.insert(List.of(new OrdinalPair(9, 1L << 32)));
        assertThrows(ArithmeticException.class, () -> client.getRoaringBitmap(9));

        assertEquals(new ServerStats(1024, 64, 128, 2, 4, 3), client.stats());
        assertTrue(
            client.supports(
                ServerStats.FEATURE_MIXED_PUT | ServerStats.FEATURE_WRITE_TRANSACTIONS));
        // A server predating the field reports zero, because protobuf decodes
        // an absent field as its default.
        assertFalse(
            new ServerStats(1024, 64, 128, 2, 4, 0).supports(ServerStats.FEATURE_MIXED_PUT));

        // The commit version the server reports, which is what makes a
        // read-your-writes read expressible: it is passed to prepareQueryAt.
        YesnoClient.IngestAck acked =
            client.insertBatchAcked(List.of(new OrdinalPair(11, 1), new OrdinalPair(11, 2)));
        assertEquals(2, acked.rows());
        assertTrue(acked.version() > 0, "a current server must report a commit version");
        long first = acked.version();
        assertTrue(
            client.insertBatchAcked(List.of(new OrdinalPair(11, 3))).version() > first,
            "a later commit must report a later version");

        // The compatibility path, and the reason this switch exists: a server
        // predating the version field sends 8 bytes, and the client must lose the
        // version rather than the call.
        producer.legacyAcknowledgementWidth = true;
        YesnoClient.IngestAck legacy =
            client.insertBatchAcked(List.of(new OrdinalPair(12, 1)));
        assertEquals(1, legacy.rows());
        assertEquals(0, legacy.version(), "an 8-byte acknowledgement reports no version");
        assertEquals(1, client.insert(List.of(new OrdinalPair(12, 2))));
      }
    }
  }

  private static NavigableSet<Long> newSet() {
    return new ConcurrentSkipListSet<>(Long::compareUnsigned);
  }

  private static final class TestProducer extends NoOpFlightProducer {
    private static final long VERSION = 9;
    private static final Field ORDINAL_FIELD =
        new Field("ordinal", FieldType.notNullable(new ArrowType.Int(64, false)), null);
    private static final Schema ORDINAL_SCHEMA = new Schema(List.of(ORDINAL_FIELD));
    private final BufferAllocator allocator;
    private final Map<Long, NavigableSet<Long>> values = new ConcurrentHashMap<>();
    private volatile SetExpression lastExpression;
    private volatile QueryRequest lastRequest;

    private TestProducer(BufferAllocator allocator) {
      this.allocator = allocator;
    }

    @Override
    public void listFlights(
        FlightProducer.CallContext context,
        Criteria criteria,
        FlightProducer.StreamListener<FlightInfo> listener) {
      values.keySet().stream()
          .sorted(Long::compareUnsigned)
          .map(this::infoForKey)
          .forEach(listener::onNext);
      listener.onCompleted();
    }

    @Override
    public FlightInfo getFlightInfo(
        FlightProducer.CallContext context, FlightDescriptor descriptor) {
      byte[] command = descriptor.getCommand();
      long key;
      if (command.length == Long.BYTES) {
        key = littleEndianLong(command);
        lastExpression = null;
        lastRequest = null;
      } else if (QueryRequest.looksLikeRequest(command)) {
        QueryRequest request = QueryRequest.decode(command);
        key = 42;
        lastExpression = request.expression();
        lastRequest = request;
      } else {
        lastExpression = SetExpression.decode(command);
        lastRequest = null;
        key = 42;
      }
      return info(descriptor, key, values.getOrDefault(key, newSet()).size());
    }

    @Override
    public void getStream(
        FlightProducer.CallContext context,
        Ticket ticket,
        FlightProducer.ServerStreamListener listener) {
      long key = ByteBuffer.wrap(ticket.getBytes()).order(ByteOrder.LITTLE_ENDIAN).getLong(8);
      List<Long> ordinals = new ArrayList<>(values.getOrDefault(key, newSet()));
      try (VectorSchemaRoot root = VectorSchemaRoot.of(new UInt8Vector(ORDINAL_FIELD, allocator))) {
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

    /** Emit the pre-2026-09-12 acknowledgement, to pin the compatibility path. */
    boolean legacyAcknowledgementWidth;

    private long commitVersion;

    @Override
    public Runnable acceptPut(
        FlightProducer.CallContext context,
        FlightStream stream,
        FlightProducer.StreamListener<PutResult> acknowledgements) {
      return () -> {
        long rows = 0;
        boolean remove =
            Arrays.equals(
                stream.getDescriptor().getCommand(), "remove".getBytes(StandardCharsets.US_ASCII));
        while (stream.next()) {
          VectorSchemaRoot root = stream.getRoot();
          UInt8Vector keys = (UInt8Vector) root.getVector("key");
          UInt8Vector ordinals = (UInt8Vector) root.getVector("ordinal");
          for (int row = 0; row < root.getRowCount(); row++) {
            NavigableSet<Long> set = values.computeIfAbsent(keys.get(row), ignored -> newSet());
            if (remove) {
              set.remove(ordinals.get(row));
            } else {
              set.add(ordinals.get(row));
            }
            rows++;
          }
        }
        // Sixteen bytes -- rows, then the commit version -- because that is what a
        // current server sends. This fixture emitted only the row count until
        // 2026-09-12, which made it encode the obsolete contract it exists to
        // validate: the client's 8-byte-only decoder passed against it while every
        // real ingest failed. `legacyAcknowledgementWidth` covers the short form.
        long width = legacyAcknowledgementWidth ? Long.BYTES : 2L * Long.BYTES;
        try (ArrowBuf metadata = allocator.buffer(width)) {
          for (int index = 0; index < Long.BYTES; index++) {
            metadata.setByte(index, (int) (rows >>> (index * Byte.SIZE)) & 0xff);
          }
          if (!legacyAcknowledgementWidth) {
            commitVersion++;
            for (int index = 0; index < Long.BYTES; index++) {
              metadata.setByte(
                  Long.BYTES + index, (int) (commitVersion >>> (index * Byte.SIZE)) & 0xff);
            }
          }
          metadata.writerIndex(width);
          metadata.getReferenceManager().retain();
          try (PutResult result = PutResult.metadata(metadata)) {
            acknowledgements.onNext(result);
          }
        }
        acknowledgements.onCompleted();
      };
    }

    @Override
    public void doAction(
        FlightProducer.CallContext context,
        Action action,
        FlightProducer.StreamListener<Result> listener) {
      byte[] body =
          switch (action.getType()) {
            case "stats" ->
                // Fields 1-5, then field 6 -- the capability bitmask.
                new byte[] {
                  0x08, (byte) 0x80, 0x08, 0x10, 0x40, 0x18, (byte) 0x80, 0x01,
                  0x20, 0x02, 0x28, 0x04, 0x30, 0x03
                };
            default -> throw new IllegalArgumentException("unknown action");
          };
      listener.onNext(new Result(body));
      listener.onCompleted();
    }

    private FlightInfo infoForKey(long key) {
      return info(FlightDescriptor.command(littleEndianBytes(key)), key, values.get(key).size());
    }

    private FlightInfo info(FlightDescriptor descriptor, long key, long records) {
      return new FlightInfo(
          ORDINAL_SCHEMA,
          descriptor,
          List.of(new FlightEndpoint(new Ticket(ticket(VERSION, key)))),
          -1,
          records);
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

  private static byte[] littleEndianBytes(long value) {
    return ByteBuffer.allocate(Long.BYTES).order(ByteOrder.LITTLE_ENDIAN).putLong(value).array();
  }

  private static long littleEndianLong(byte[] value) {
    return ByteBuffer.wrap(value).order(ByteOrder.LITTLE_ENDIAN).getLong();
  }

  @org.junit.jupiter.api.Test
  void aMutationIsValidatedOnEveryConstructionPath() {
    // The factories are not the only way in; this is a record.
    assertThrows(
        IllegalArgumentException.class, () -> new Mutation(1, 2, 3, Mutation.OP_INSERT));
    assertThrows(
        IllegalArgumentException.class, () -> new Mutation(1, 5, 0, Mutation.OP_DELETE_KEY));
    assertThrows(
        IllegalArgumentException.class, () -> new Mutation(1, 9, 4, Mutation.OP_INSERT_RANGE));
    assertThrows(IllegalArgumentException.class, () -> new Mutation(1, 0, 0, (byte) 99));
    assertThrows(
        IllegalArgumentException.class, () -> new Mutation(1, -1L, -1L, Mutation.OP_INSERT));

    assertEquals(new Mutation(1, 2, 2, Mutation.OP_INSERT), Mutation.insert(1, 2));
    assertEquals(new Mutation(7, 0, 0, Mutation.OP_DELETE_KEY), Mutation.deleteKey(7));
    assertEquals(new Mutation(1, 5, 9, Mutation.OP_INSERT_RANGE), Mutation.insertRange(1, 5, 9));
  }
}
