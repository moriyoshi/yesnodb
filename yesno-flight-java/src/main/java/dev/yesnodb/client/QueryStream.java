package dev.yesnodb.client;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import org.apache.arrow.flight.FlightStream;
import org.apache.arrow.vector.FieldVector;
import org.apache.arrow.vector.UInt8Vector;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.roaringbitmap.RoaringBitmap;

/** A planned query and its decoded Arrow record-batch stream. */
public final class QueryStream implements AutoCloseable {
  private final QueryInfo info;
  private final FlightStream stream;
  private long received;
  private boolean positioned;
  private boolean finished;

  QueryStream(QueryInfo info, FlightStream stream) {
    this.info = Objects.requireNonNull(info, "info");
    this.stream = Objects.requireNonNull(stream, "stream");
  }

  /** The exact metadata and versioned ticket for this stream. */
  public QueryInfo info() {
    return info;
  }

  /**
   * Advance to the next batch.
   *
   * @return {@code true} when {@link #root()} contains another batch
   */
  public boolean next() {
    if (finished) {
      return false;
    }
    boolean hasNext = stream.next();
    positioned = hasNext;
    if (!hasNext) {
      finished = true;
      if (received != info.totalRecords()) {
        throw new IllegalStateException(
            "yesnodb returned " + received + " ordinals after promising " + info.totalRecords());
      }
      return false;
    }
    VectorSchemaRoot root = stream.getRoot();
    ordinalVector(root);
    received = Math.addExact(received, root.getRowCount());
    return true;
  }

  /** Return the current Arrow batch; valid only after {@link #next()} returns {@code true}. */
  public VectorSchemaRoot root() {
    if (!positioned) {
      throw new IllegalStateException("next() has not positioned the query on a batch");
    }
    return stream.getRoot();
  }

  /** Materialize all remaining ordinals as raw unsigned 64-bit {@code long} values. */
  public List<Long> collectOrdinals() {
    List<Long> ordinals = new ArrayList<>();
    while (next()) {
      VectorSchemaRoot root = root();
      UInt8Vector vector = ordinalVector(root);
      for (int row = 0; row < root.getRowCount(); row++) {
        ordinals.add(vector.get(row));
      }
    }
    return ordinals;
  }

  /**
   * Materialize all remaining ordinals as a 32-bit {@link RoaringBitmap}.
   *
   * <p>This conversion is loss-checked. It throws if the result contains an ordinal above {@code
   * 2^32 - 1}; use {@link #collectOrdinals()} or batch streaming for the native 64-bit domain.
   */
  public RoaringBitmap collectRoaringBitmap() {
    RoaringBitmap bitmap = new RoaringBitmap();
    while (next()) {
      VectorSchemaRoot root = root();
      UInt8Vector vector = ordinalVector(root);
      for (int row = 0; row < root.getRowCount(); row++) {
        long ordinal = vector.get(row);
        if (Long.compareUnsigned(ordinal, 0xffff_ffffL) > 0) {
          throw new ArithmeticException(
              "ordinal "
                  + Long.toUnsignedString(ordinal)
                  + " does not fit in a 32-bit RoaringBitmap");
        }
        bitmap.add((int) ordinal);
      }
    }
    return bitmap;
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
      throw new IllegalStateException("yesnodb returned nulls in its non-null `ordinal` column");
    }
    return ordinal;
  }

  /** Close the Flight stream and release its Arrow buffers. */
  @Override
  public void close() {
    try {
      stream.close();
    } catch (RuntimeException exception) {
      throw exception;
    } catch (Exception exception) {
      throw new IllegalStateException("could not close the yesnodb query stream", exception);
    }
  }
}
