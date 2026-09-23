package dev.yesnodb.client;

import com.google.protobuf.CodedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.CountDownLatch;
import org.apache.arrow.flight.Action;
import org.apache.arrow.flight.CallOption;
import org.apache.arrow.flight.Criteria;
import org.apache.arrow.flight.FlightClient;
import org.apache.arrow.flight.FlightDescriptor;
import org.apache.arrow.flight.FlightInfo;
import org.apache.arrow.flight.FlightStream;
import org.apache.arrow.flight.Location;
import org.apache.arrow.flight.PutResult;
import org.apache.arrow.flight.Ticket;
import org.apache.arrow.memory.ArrowBuf;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.memory.RootAllocator;
import org.apache.arrow.vector.UInt1Vector;
import org.apache.arrow.vector.UInt8Vector;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.apache.arrow.vector.types.pojo.ArrowType;
import org.apache.arrow.vector.types.pojo.Field;
import org.apache.arrow.vector.types.pojo.FieldType;
import org.roaringbitmap.IntIterator;
import org.roaringbitmap.RoaringBitmap;

/**
 * A synchronous, yesnodb-shaped client over Apache Arrow Flight.
 *
 * <p>Instances own their underlying {@link FlightClient} and are not safe for concurrent calls. The
 * no-allocator {@link #connect(Location)} factory also owns its root allocator. All key and ordinal
 * {@code long} arguments carry raw unsigned 64-bit bits.
 */
public final class YesnoClient implements AutoCloseable {
  private static final int BATCH_ROWS = 8192;
  private static final byte[] PUT_INSERT = "insert".getBytes(StandardCharsets.US_ASCII);
  private static final byte[] PUT_REMOVE = "remove".getBytes(StandardCharsets.US_ASCII);
  private static final Field KEY_FIELD =
      new Field("key", FieldType.notNullable(new ArrowType.Int(64, false)), null);
  private static final Field ORDINAL_FIELD =
      new Field("ordinal", FieldType.notNullable(new ArrowType.Int(64, false)), null);
  private static final byte[] PUT_APPLY = "apply".getBytes(StandardCharsets.US_ASCII);
  private static final byte[] PUT_TXN_PREFIX = "txn:".getBytes(StandardCharsets.US_ASCII);
  private static final Field LO_FIELD =
      new Field("lo", FieldType.notNullable(new ArrowType.Int(64, false)), null);
  private static final Field HI_FIELD =
      new Field("hi", FieldType.notNullable(new ArrowType.Int(64, false)), null);
  private static final Field OP_FIELD =
      new Field("op", FieldType.notNullable(new ArrowType.Int(8, false)), null);

  private final BufferAllocator allocator;
  private final FlightClient flight;
  private final RootAllocator ownedAllocator;
  private boolean closed;

  /**
   * Wrap an existing allocator and Flight client.
   *
   * <p>This wrapper owns and closes {@code flight}; the caller retains ownership of {@code
   * allocator}.
   */
  public YesnoClient(BufferAllocator allocator, FlightClient flight) {
    this(allocator, flight, null);
  }

  private YesnoClient(
      BufferAllocator allocator, FlightClient flight, RootAllocator ownedAllocator) {
    this.allocator = Objects.requireNonNull(allocator, "allocator");
    this.flight = Objects.requireNonNull(flight, "flight");
    this.ownedAllocator = ownedAllocator;
  }

  /** Connect with an internally owned root allocator. */
  public static YesnoClient connect(Location location) {
    Objects.requireNonNull(location, "location");
    RootAllocator allocator = new RootAllocator();
    try {
      FlightClient flight = FlightClient.builder(allocator, location).build();
      return new YesnoClient(allocator, flight, allocator);
    } catch (RuntimeException | Error exception) {
      allocator.close();
      throw exception;
    }
  }

  /** Connect with a caller-owned allocator. */
  public static YesnoClient connect(BufferAllocator allocator, Location location) {
    Objects.requireNonNull(allocator, "allocator");
    Objects.requireNonNull(location, "location");
    return new YesnoClient(allocator, FlightClient.builder(allocator, location).build());
  }

  /** Access the underlying Flight client, for authentication and custom metadata. */
  public FlightClient flightClient() {
    ensureOpen();
    return flight;
  }

  /** Return every populated key in ascending unsigned order. */
  public List<Long> keys(CallOption... options) {
    ensureOpen();
    List<Long> keys = new ArrayList<>();
    for (FlightInfo info : flight.listFlights(Criteria.ALL, options)) {
      FlightDescriptor descriptor = info.getDescriptor();
      if (!descriptor.isCommand() || descriptor.getCommand().length != Long.BYTES) {
        throw new IllegalStateException("yesnodb key listing returned a malformed descriptor");
      }
      keys.add(littleEndianLong(descriptor.getCommand()));
    }
    return List.copyOf(keys);
  }

  /** Plan one key lookup without fetching any ordinals. */
  public QueryInfo prepareKey(long key, CallOption... options) {
    return prepareCommand(littleEndianBytes(key), options);
  }

  /** Plan one expression without fetching any ordinals. */
  public QueryInfo prepareQuery(SetExpression expression, CallOption... options) {
    Objects.requireNonNull(expression, "expression");
    return prepareCommand(expression.encode(), options);
  }

  /** Plan one expression at exactly the caller-selected database version. */
  public QueryInfo prepareQueryAt(SetExpression expression, long version, CallOption... options) {
    Objects.requireNonNull(expression, "expression");
    return prepareCommand(QueryRequest.at(expression, version).encode(), options);
  }

  /** Plan a current or version-pinned query request. */
  public QueryInfo prepareQuery(QueryRequest request, CallOption... options) {
    return prepareCommand(Objects.requireNonNull(request, "request").encode(), options);
  }

  /** Plan an already encoded yesnodb descriptor command. */
  public QueryInfo prepareCommand(byte[] command, CallOption... options) {
    ensureOpen();
    Objects.requireNonNull(command, "command");
    return QueryInfo.from(flight.getInfo(FlightDescriptor.command(command.clone()), options));
  }

  /** Return one key's exact cardinality without fetching ordinals. */
  public long cardinality(long key, CallOption... options) {
    return prepareKey(key, options).totalRecords();
  }

  /** Return one expression's exact cardinality without fetching ordinals. */
  public long queryCardinality(SetExpression expression, CallOption... options) {
    return prepareQuery(expression, options).totalRecords();
  }

  /** Fetch a previously planned query at the version named by its ticket. */
  public FlightStream fetch(QueryInfo query, CallOption... options) {
    ensureOpen();
    Objects.requireNonNull(query, "query");
    return flight.getStream(query.flightTicket(), options);
  }

  /** Fetch opaque ticket bytes previously returned by this yesnodb server. */
  public FlightStream fetchTicket(byte[] ticket, CallOption... options) {
    ensureOpen();
    Objects.requireNonNull(ticket, "ticket");
    return flight.getStream(new Ticket(ticket.clone()), options);
  }

  /** Plan and fetch one key. */
  public QueryStream get(long key, CallOption... options) {
    QueryInfo info = prepareKey(key, options);
    return new QueryStream(info, fetch(info, options));
  }

  /** Plan and fetch one Boolean expression. */
  public QueryStream query(SetExpression expression, CallOption... options) {
    QueryInfo info = prepareQuery(expression, options);
    return new QueryStream(info, fetch(info, options));
  }

  /** Plan, fetch, and loss-check one key into a 32-bit {@link RoaringBitmap}. */
  public RoaringBitmap getRoaringBitmap(long key, CallOption... options) {
    try (QueryStream query = get(key, options)) {
      return query.collectRoaringBitmap();
    }
  }

  /** Plan, fetch, and loss-check one expression into a 32-bit {@link RoaringBitmap}. */
  public RoaringBitmap queryRoaringBitmap(SetExpression expression, CallOption... options) {
    try (QueryStream query = query(expression, options)) {
      return query.collectRoaringBitmap();
    }
  }

  /** Insert pairs in bounded Arrow batches, returning the acknowledged count. */
  public long insert(Iterable<OrdinalPair> pairs, CallOption... options) {
    return put(pairs, PUT_INSERT, BATCH_ROWS, options);
  }

  /** Remove pairs in bounded Arrow batches, returning the acknowledged count. */
  public long remove(Iterable<OrdinalPair> pairs, CallOption... options) {
    return put(pairs, PUT_REMOVE, BATCH_ROWS, options);
  }

  /**
   * Insert all pairs in one batch and one commit, reporting the commit version with the count.
   *
   * <p>This is the read-your-writes primitive: pass {@link IngestAck#version()} when planning a
   * query at a caller-selected version and the read is bound to a database state containing the
   * write. One batch is one commit, so the version names exactly this call's write.
   */
  public IngestAck insertBatchAcked(Iterable<OrdinalPair> pairs, CallOption... options) {
    return putOneBatchAcked(pairs, PUT_INSERT, options);
  }

  /** {@link #insertBatchAcked} for removal. */
  public IngestAck removeBatchAcked(Iterable<OrdinalPair> pairs, CallOption... options) {
    return putOneBatchAcked(pairs, PUT_REMOVE, options);
  }

  /** Insert every unsigned 32-bit value from {@code bitmap} under one key. */
  public long insert(long key, RoaringBitmap bitmap, CallOption... options) {
    return insert(roaringPairs(key, bitmap), options);
  }

  /** Remove every unsigned 32-bit value from {@code bitmap} under one key. */
  public long remove(long key, RoaringBitmap bitmap, CallOption... options) {
    return remove(roaringPairs(key, bitmap), options);
  }

  /** Insert every supplied pair in one Arrow batch and one server-side commit. */
  public long insertBatch(Iterable<OrdinalPair> pairs, CallOption... options) {
    return putOneBatch(pairs, PUT_INSERT, options);
  }

  /** Remove every supplied pair in one Arrow batch and one server-side commit. */
  public long removeBatch(Iterable<OrdinalPair> pairs, CallOption... options) {
    return putOneBatch(pairs, PUT_REMOVE, options);
  }

  /** Insert one bitmap in one Arrow batch and one server-side commit. */
  public long insertBatch(long key, RoaringBitmap bitmap, CallOption... options) {
    return insertBatch(roaringPairs(key, bitmap), options);
  }

  /** Remove one bitmap in one Arrow batch and one server-side commit. */
  public long removeBatch(long key, RoaringBitmap bitmap, CallOption... options) {
    return removeBatch(roaringPairs(key, bitmap), options);
  }

  /** Return typed server space and reader counters. */
  /**
   * Whether the server implements every bit in {@code features}.
   *
   * <p>Ask before calling {@link #apply} or {@link #beginWrite}. A server predating the capability
   * field reports zero and treats an unrecognised DoPut command as insert, so an unchecked mixed
   * batch would have its removals applied as insertions with no error anywhere.
   */
  public boolean supports(long features, CallOption... options) {
    return stats(options).supports(features);
  }

  /**
   * Applies mixed operations as <b>one</b> commit.
   *
   * <p>Every batch accumulates into a single write batch that commits once at end of stream, so the
   * returned version is the single instant at which every row became visible. Operations apply in
   * the order given, which is what lets a change-data feed replay a delete-then-reinsert of one key
   * without the two reordering. This is the difference from {@link #insert}, which commits per
   * record batch and can only report the last of several versions.
   */
  public IngestAck apply(Iterable<Mutation> mutations, CallOption... options) {
    return putMutations(mutations, PUT_APPLY, options);
  }

  /** Opens a write transaction spanning several calls. */
  public long beginWrite(CallOption... options) {
    return decodeLong("begin_write", action("begin_write", options));
  }

  /**
   * Stages mutations into an open transaction, returning the rows accepted.
   *
   * <p>Staged rows are invisible to readers until {@link #commitWrite}.
   *
   * <p>A staging error is fatal to the whole transaction. Mutations are sent in batches, and a
   * failure in a later batch leaves earlier ones staged with no way to withdraw them, so the
   * server poisons the transaction: {@link #commitWrite} will refuse it and {@link #abortWrite} is
   * the only way out. Do not retry a failed stage in place. A mutation this client can reject --
   * everything {@link Mutation} validates -- never reaches the server, so it costs nothing.
   */
  public long stage(long txn, Iterable<Mutation> mutations, CallOption... options) {
    byte[] command = new byte[PUT_TXN_PREFIX.length + 8];
    System.arraycopy(PUT_TXN_PREFIX, 0, command, 0, PUT_TXN_PREFIX.length);
    ByteBuffer.wrap(command, PUT_TXN_PREFIX.length, 8).order(ByteOrder.LITTLE_ENDIAN).putLong(txn);
    return putMutations(mutations, command, options).rows();
  }

  /**
   * Publishes everything staged in {@code txn} and returns its version.
   *
   * <p>Idempotent within one live service, and only there. The server remembers a bounded number
   * of recent outcomes in memory, so a prompt retry after a lost response returns the original
   * version rather than committing twice. A restart loses every outcome and later traffic evicts
   * older ones, so this does not cover the case a change-data pipe has to survive: losing the
   * response and then finding the server restarted. Such a caller cannot learn the version its
   * transaction committed at. Handles do not recur, so replaying the old one fails closed rather
   * than resolving a different transaction, but failing closed is not recovering.
   */
  public long commitWrite(long txn, CallOption... options) {
    return decodeLong("commit_write", action("commit_write", longBody(txn), options));
  }

  /**
   * Discards everything staged in {@code txn}.
   *
   * <p>Aborting a transaction the server no longer knows about succeeds: it was already aborted or
   * it expired, and either way the caller's intent -- that none of it is visible -- already holds.
   * Aborting one that committed is an error, because a version exists that says otherwise.
   */
  public void abortWrite(long txn, CallOption... options) {
    action("abort_write", longBody(txn), options);
  }

  public ServerStats stats(CallOption... options) {
    return decodeStats(action("stats", options));
  }

  private static Iterable<OrdinalPair> roaringPairs(long key, RoaringBitmap bitmap) {
    Objects.requireNonNull(bitmap, "bitmap");
    return () -> {
      IntIterator ordinals = bitmap.getIntIterator();
      return new Iterator<>() {
        @Override
        public boolean hasNext() {
          return ordinals.hasNext();
        }

        @Override
        public OrdinalPair next() {
          return new OrdinalPair(key, Integer.toUnsignedLong(ordinals.next()));
        }
      };
    };
  }

  private long putOneBatch(Iterable<OrdinalPair> pairs, byte[] command, CallOption... options) {
    Objects.requireNonNull(pairs, "pairs");
    List<OrdinalPair> materialized = new ArrayList<>();
    pairs.forEach(pair -> materialized.add(Objects.requireNonNull(pair, "pair")));
    return put(materialized, command, Math.max(1, materialized.size()), options);
  }

  private IngestAck putOneBatchAcked(
      Iterable<OrdinalPair> pairs, byte[] command, CallOption... options) {
    Objects.requireNonNull(pairs, "pairs");
    List<OrdinalPair> materialized = new ArrayList<>();
    pairs.forEach(pair -> materialized.add(Objects.requireNonNull(pair, "pair")));
    return putAcked(materialized, command, Math.max(1, materialized.size()), options);
  }

  private long put(
      Iterable<OrdinalPair> pairs, byte[] command, int batchRows, CallOption... options) {
    return putAcked(pairs, command, batchRows, options).rows();
  }

  private IngestAck putAcked(
      Iterable<OrdinalPair> pairs, byte[] command, int batchRows, CallOption... options) {
    ensureOpen();
    Objects.requireNonNull(pairs, "pairs");
    Iterator<OrdinalPair> iterator = pairs.iterator();
    AckListener acknowledgements = new AckListener();
    long sent = 0;

    try (VectorSchemaRoot root =
        VectorSchemaRoot.of(
            new UInt8Vector(KEY_FIELD, allocator), new UInt8Vector(ORDINAL_FIELD, allocator))) {
      UInt8Vector keys = (UInt8Vector) root.getVector("key");
      UInt8Vector ordinals = (UInt8Vector) root.getVector("ordinal");
      keys.allocateNew(batchRows);
      ordinals.allocateNew(batchRows);
      FlightClient.ClientStreamListener writer =
          flight.startPut(FlightDescriptor.command(command), root, acknowledgements, options);
      try {
        while (iterator.hasNext()) {
          int row = 0;
          while (row < batchRows && iterator.hasNext()) {
            OrdinalPair pair = Objects.requireNonNull(iterator.next(), "pair");
            keys.set(row, pair.key());
            ordinals.set(row, pair.ordinal());
            row++;
          }
          keys.setValueCount(row);
          ordinals.setValueCount(row);
          root.setRowCount(row);
          writer.putNext();
          sent = Math.addExact(sent, row);
        }
      } catch (RuntimeException | Error exception) {
        writer.error(exception);
        throw exception;
      }
      writer.completed();
      writer.getResult();
    }

    IngestAck acknowledged = acknowledgements.ack();
    if (acknowledged.rows() != sent) {
      throw new IllegalStateException(
          "yesnodb acknowledged " + acknowledged.rows() + " of " + sent + " ingest pairs");
    }
    return acknowledged;
  }

  private byte[] action(String name, CallOption... options) {
    return action(name, new byte[0], options);
  }

  private byte[] action(String name, byte[] request, CallOption... options) {
    ensureOpen();
    ByteArrayOutputStream body = new ByteArrayOutputStream();
    Iterator<org.apache.arrow.flight.Result> chunks =
        flight.doAction(new Action(name, request), options);
    chunks.forEachRemaining(result -> body.writeBytes(result.getBody()));
    return body.toByteArray();
  }

  private static byte[] longBody(long value) {
    byte[] body = new byte[8];
    ByteBuffer.wrap(body).order(ByteOrder.LITTLE_ENDIAN).putLong(value);
    return body;
  }

  private static long decodeLong(String name, byte[] body) {
    if (body.length != 8) {
      throw new IllegalStateException(
          "yesnodb " + name + " returned " + body.length + " bytes instead of 8");
    }
    return ByteBuffer.wrap(body).order(ByteOrder.LITTLE_ENDIAN).getLong();
  }

  private IngestAck putMutations(
      Iterable<Mutation> mutations, byte[] command, CallOption... options) {
    ensureOpen();
    Objects.requireNonNull(mutations, "mutations");
    Iterator<Mutation> iterator = mutations.iterator();
    AckListener acknowledgements = new AckListener();
    long sent = 0;

    try (VectorSchemaRoot root =
        VectorSchemaRoot.of(
            new UInt8Vector(KEY_FIELD, allocator),
            new UInt8Vector(LO_FIELD, allocator),
            new UInt8Vector(HI_FIELD, allocator),
            new UInt1Vector(OP_FIELD, allocator))) {
      UInt8Vector keys = (UInt8Vector) root.getVector("key");
      UInt8Vector lo = (UInt8Vector) root.getVector("lo");
      UInt8Vector hi = (UInt8Vector) root.getVector("hi");
      UInt1Vector op = (UInt1Vector) root.getVector("op");
      keys.allocateNew(BATCH_ROWS);
      lo.allocateNew(BATCH_ROWS);
      hi.allocateNew(BATCH_ROWS);
      op.allocateNew(BATCH_ROWS);
      FlightClient.ClientStreamListener writer =
          flight.startPut(FlightDescriptor.command(command), root, acknowledgements, options);
      try {
        while (iterator.hasNext()) {
          int row = 0;
          while (row < BATCH_ROWS && iterator.hasNext()) {
            Mutation mutation = Objects.requireNonNull(iterator.next(), "mutation");
            keys.set(row, mutation.key());
            lo.set(row, mutation.lo());
            hi.set(row, mutation.hi());
            op.set(row, mutation.op());
            row++;
          }
          keys.setValueCount(row);
          lo.setValueCount(row);
          hi.setValueCount(row);
          op.setValueCount(row);
          root.setRowCount(row);
          writer.putNext();
          sent = Math.addExact(sent, row);
        }
      } catch (RuntimeException | Error exception) {
        writer.error(exception);
        throw exception;
      }
      writer.completed();
      writer.getResult();
    }

    IngestAck acknowledged = acknowledgements.ack();
    if (acknowledged.rows() != sent) {
      throw new IllegalStateException(
          "yesnodb acknowledged " + acknowledged.rows() + " of " + sent + " mutations");
    }
    return acknowledged;
  }

  private static ServerStats decodeStats(byte[] body) {
    CodedInputStream input = CodedInputStream.newInstance(body);
    long allocatedBytes = 0;
    long deferredBytes = 0;
    long walBytes = 0;
    long liveReaders = 0;
    long shards = 0;
    long features = 0;
    try {
      while (!input.isAtEnd()) {
        int tag = input.readTag();
        switch (tag) {
          case 8 -> allocatedBytes = input.readUInt64();
          case 16 -> deferredBytes = input.readUInt64();
          case 24 -> walBytes = input.readUInt64();
          case 32 -> liveReaders = input.readUInt64();
          case 40 -> shards = input.readUInt64();
          case 48 -> features = input.readUInt64();
          default -> {
            if (!input.skipField(tag)) {
              throw new IOException("unexpected end-group tag");
            }
          }
        }
      }
    } catch (IOException exception) {
      throw new IllegalStateException("yesnodb stats returned invalid protobuf", exception);
    }
    return new ServerStats(allocatedBytes, deferredBytes, walBytes, liveReaders, shards, features);
  }

  private static byte[] littleEndianBytes(long value) {
    return ByteBuffer.allocate(Long.BYTES).order(ByteOrder.LITTLE_ENDIAN).putLong(value).array();
  }

  private static long littleEndianLong(byte[] value) {
    if (value.length != Long.BYTES) {
      throw new IllegalArgumentException("expected exactly 8 bytes");
    }
    return ByteBuffer.wrap(value).order(ByteOrder.LITTLE_ENDIAN).getLong();
  }

  private void ensureOpen() {
    if (closed) {
      throw new IllegalStateException("yesnodb client is closed");
    }
  }

  /** Close the Flight transport and any internally owned allocator. */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    closed = true;
    try {
      try {
        flight.close();
      } catch (InterruptedException exception) {
        Thread.currentThread().interrupt();
        throw new IllegalStateException("interrupted while closing the Flight client", exception);
      }
    } finally {
      if (ownedAllocator != null) {
        ownedAllocator.close();
      }
    }
  }

  /** Copies acknowledgement metadata before Arrow reclaims the callback buffer. */
  private static final class AckListener implements FlightClient.PutListener {
    private final CountDownLatch completed = new CountDownLatch(1);
    private volatile Throwable failure;
    private volatile boolean hasAcknowledgement;
    private volatile long acknowledgement;
    private volatile long commitVersion;

    /**
     * Decodes the ingest acknowledgement, which is eight or sixteen bytes.
     *
     * <p>It carried eight -- the row count alone -- until 2026-09-12, when the commit version was
     * appended. Both widths are accepted so a current client keeps working against a server that
     * predates the version field: such a server loses the version, not the call. A reported version
     * of zero means the server committed nothing, because no commit is ever assigned version zero.
     */
    @Override
    public void onNext(PutResult result) {
      ArrowBuf metadata = result.getApplicationMetadata();
      long width = metadata == null ? -1 : metadata.readableBytes();
      if (width != Long.BYTES && width != 2 * Long.BYTES) {
        failure =
            new IllegalStateException(
                "yesnodb ingest acknowledgement must contain 8 or 16 bytes, got " + width);
        return;
      }
      long offset = metadata.readerIndex();
      acknowledgement = readLittleEndianLong(metadata, offset);
      commitVersion = width == 2 * Long.BYTES ? readLittleEndianLong(metadata, offset + Long.BYTES) : 0;
      hasAcknowledgement = true;
    }

    private static long readLittleEndianLong(ArrowBuf buffer, long offset) {
      long value = 0;
      for (int index = 0; index < Long.BYTES; index++) {
        value |= (long) Byte.toUnsignedInt(buffer.getByte(offset + index)) << (index * Byte.SIZE);
      }
      return value;
    }

    @Override
    public void onError(Throwable throwable) {
      failure = throwable;
      completed.countDown();
    }

    @Override
    public void onCompleted() {
      completed.countDown();
    }

    @Override
    public void getResult() {
      try {
        completed.await();
      } catch (InterruptedException exception) {
        Thread.currentThread().interrupt();
        throw new IllegalStateException("interrupted while waiting for yesnodb", exception);
      }
      if (failure instanceof RuntimeException runtime) {
        throw runtime;
      }
      if (failure != null) {
        throw new IllegalStateException("yesnodb ingest failed", failure);
      }
    }

    @Override
    public boolean isCancelled() {
      return failure != null;
    }

    private long acknowledged() {
      return ack().rows();
    }

    private IngestAck ack() {
      getResult();
      if (!hasAcknowledgement) {
        throw new IllegalStateException("yesnodb returned no ingest acknowledgement");
      }
      return new IngestAck(acknowledgement, commitVersion);
    }
  }

  /**
   * What the server acknowledged for one ingest call.
   *
   * <p>{@code version} is zero when the server reported none, which happens against a server
   * predating the version field and when an ingest committed nothing. It is never a readable version
   * zero: that is the empty database, and no commit is assigned it.
   */
  public record IngestAck(long rows, long version) {}
}
