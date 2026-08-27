package dev.yesnodb.search;

import dev.yesnodb.client.QueryInfo;
import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.Objects;
import org.apache.arrow.flight.CallOption;
import org.apache.arrow.flight.FlightRuntimeException;
import org.apache.arrow.flight.FlightStatusCode;
import org.apache.arrow.flight.FlightStream;
import org.apache.arrow.vector.FieldVector;
import org.apache.arrow.vector.UInt8Vector;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.roaringbitmap.RoaringBitmap;
import org.roaringbitmap.longlong.Roaring64NavigableMap;

/** Resolves one yesnodb expression into a bounded, portable search bitmap. */
public final class YesnoBitmapResolver {
  private final YesnoClient client;

  /** Wrap a caller-owned client. The resolver does not close it and is not thread-safe. */
  public YesnoBitmapResolver(YesnoClient client) {
    this.client = Objects.requireNonNull(client, "client");
  }

  /** Resolve with the default resource limits. */
  public PreparedBitmap resolve(
      SetExpression expression,
      BitmapWidth width,
      SnapshotMode snapshot,
      CallOption... options) {
    return resolve(expression, width, snapshot, ResolveLimits.defaults(), options);
  }

  /** Plan, bound, fetch, validate, and portably serialize one yesnodb result. */
  public PreparedBitmap resolve(
      SetExpression expression,
      BitmapWidth width,
      SnapshotMode snapshot,
      ResolveLimits limits,
      CallOption... options) {
    Objects.requireNonNull(expression, "expression");
    Objects.requireNonNull(width, "width");
    Objects.requireNonNull(snapshot, "snapshot");
    Objects.requireNonNull(limits, "limits");
    Objects.requireNonNull(options, "options");

    int retries = 0;
    while (true) {
      try {
        return resolveOnce(expression, width, snapshot, limits, options);
      } catch (FlightRuntimeException exception) {
        if (!(snapshot instanceof SnapshotMode.Current current)
            || retries >= current.maxRetries()
            || !retryable(exception)) {
          throw exception;
        }
        retries++;
      }
    }
  }

  // FlightStream.close exposes InterruptedException through AutoCloseable; close failures are
  // deliberately translated to a resolver failure below.
  @SuppressWarnings("try")
  private PreparedBitmap resolveOnce(
      SetExpression expression,
      BitmapWidth width,
      SnapshotMode snapshot,
      ResolveLimits limits,
      CallOption[] options) {
    QueryInfo info;
    if (snapshot instanceof SnapshotMode.Pinned pinned) {
      info = client.prepareQueryAt(expression, pinned.version(), options);
    } else {
      info = client.prepareQuery(expression, options);
    }
    long promised = info.totalRecords();
    if (promised > limits.maxMatches()) {
      throw new ResultLimitExceededException(promised, limits.maxMatches());
    }

    BitmapAccumulator accumulator =
        switch (width) {
          case INTEGER_32 -> new Bitmap32Accumulator();
          case LONG_64 -> new Bitmap64Accumulator();
        };
    long received = 0;
    boolean havePrevious = false;
    long previous = 0;

    try (FlightStream stream = client.fetch(info, options)) {
      while (stream.next()) {
        VectorSchemaRoot root = stream.getRoot();
        UInt8Vector ordinals = ordinalVector(root);
        for (int row = 0; row < root.getRowCount(); row++) {
          long ordinal = ordinals.get(row);
          if (havePrevious && Long.compareUnsigned(ordinal, previous) <= 0) {
            throw new IllegalStateException(
                "yesnodb ordinals are not strictly increasing: "
                    + Long.toUnsignedString(previous)
                    + " then "
                    + Long.toUnsignedString(ordinal));
          }
          accumulator.add(ordinal);
          previous = ordinal;
          havePrevious = true;
          received = Math.addExact(received, 1);
          if (received > promised) {
            throw cardinalityMismatch(promised, received);
          }
        }
      }
    } catch (RuntimeException exception) {
      throw exception;
    } catch (Exception exception) {
      throw new IllegalStateException("could not close the yesnodb Flight stream", exception);
    }

    if (received != promised) {
      throw cardinalityMismatch(promised, received);
    }
    byte[] bytes = accumulator.serialize();
    if (bytes.length > limits.maxSerializedBytes()) {
      throw new BitmapSizeLimitExceededException(bytes.length, limits.maxSerializedBytes());
    }
    return new PreparedBitmap(width, info.version(), received, bytes);
  }

  private static UInt8Vector ordinalVector(VectorSchemaRoot root) {
    if (root.getFieldVectors().size() != 1) {
      throw new IllegalStateException(
          "yesnodb returned " + root.getFieldVectors().size() + " columns instead of 1");
    }
    FieldVector field = root.getVector("ordinal");
    if (!(field instanceof UInt8Vector ordinal)) {
      throw new IllegalStateException("yesnodb returned no UInt64 `ordinal` column");
    }
    if (ordinal.getNullCount() != 0) {
      throw new IllegalStateException("yesnodb returned null ordinals");
    }
    return ordinal;
  }

  private static IllegalStateException cardinalityMismatch(long promised, long received) {
    return new IllegalStateException(
        "yesnodb promised " + promised + " ordinals but returned " + received);
  }

  private static boolean retryable(FlightRuntimeException exception) {
    FlightStatusCode code = exception.status().code();
    return code == FlightStatusCode.UNAVAILABLE || code == FlightStatusCode.TIMED_OUT;
  }

  private interface BitmapAccumulator {
    void add(long ordinal);

    byte[] serialize();
  }

  private static final class Bitmap32Accumulator implements BitmapAccumulator {
    private final RoaringBitmap bitmap = new RoaringBitmap();

    @Override
    public void add(long ordinal) {
      if (ordinal < 0 || ordinal > Integer.MAX_VALUE) {
        throw new OrdinalOutOfRangeException(ordinal, BitmapWidth.INTEGER_32);
      }
      bitmap.add((int) ordinal);
    }

    @Override
    public byte[] serialize() {
      return serializeToBytes(bitmap::serialize);
    }
  }

  private static final class Bitmap64Accumulator implements BitmapAccumulator {
    private final Roaring64NavigableMap bitmap = new Roaring64NavigableMap();

    @Override
    public void add(long ordinal) {
      if (ordinal < 0) {
        throw new OrdinalOutOfRangeException(ordinal, BitmapWidth.LONG_64);
      }
      bitmap.addLong(ordinal);
    }

    @Override
    public byte[] serialize() {
      return serializeToBytes(bitmap::serializePortable);
    }
  }

  private static byte[] serializeToBytes(Serializer serializer) {
    ByteArrayOutputStream bytes = new ByteArrayOutputStream();
    try (DataOutputStream output = new DataOutputStream(bytes)) {
      serializer.serialize(output);
    } catch (IOException exception) {
      throw new IllegalStateException("could not serialize the search bitmap", exception);
    }
    return bytes.toByteArray();
  }

  @FunctionalInterface
  private interface Serializer {
    void serialize(DataOutputStream output) throws IOException;
  }
}
