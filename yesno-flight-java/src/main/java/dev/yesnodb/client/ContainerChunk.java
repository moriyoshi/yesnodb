package dev.yesnodb.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Objects;
import org.roaringbitmap.IntIterator;
import org.roaringbitmap.RoaringBitmap;

/**
 * One row of the container-payload DoPut schema {@code ( key, prefix48, kind, cardinality, payload
 * )}: up to 65,536 ordinals of one key as the Roaring-spec bytes the page store already holds.
 *
 * <p>The pair and mutation wires cost one row per set bit. A container row costs one row per
 * 65,536-ordinal chunk, which the server measured at 64.8x less wire on a half-dense page. Send
 * these with {@link YesnoClient#applyContainers}, after checking {@link
 * ServerStats#FEATURE_CONTAINER_PUT}.
 *
 * <p>Build them with {@link #encode}, {@link #ofOrdinals} or {@link #ofBitmap} rather than the
 * constructor. The constructor checks what a row can be checked for alone -- the kind, the
 * cardinality's range, the prefix's width -- but not that the payload holds the set it claims; the
 * server's decoder checks that, and refuses a wrong payload as {@code INVALID_ARGUMENT}.
 *
 * <p>Insert only. A container row unions into whatever is already under the key: the write wire
 * has no op column, because giving it one would fork it from the read side's schema. A removal is
 * {@link Mutation#removeRange} on the mutation wire.
 *
 * <p>{@code key} and {@code prefix48} carry raw unsigned 64-bit bits, as elsewhere in this client;
 * the chunk holds the ordinals {@code (prefix48 << 16) | v} for each 16-bit value {@code v}.
 */
public record ContainerChunk(long key, long prefix48, byte kind, int cardinality, byte[] payload) {

  /** How many of an ordinal's low bits index within one container. */
  public static final int CHUNK_BITS = 16;

  /** How many ordinals one container addresses. */
  public static final int CHUNK_CARD = 1 << CHUNK_BITS;

  /** The highest cardinality an array payload may carry, whatever its byte count says. */
  public static final int ARRAY_MAX = 4096;

  /** A bitmap payload's length, whatever its cardinality. */
  public static final int BITMAP_BYTES = 8192;

  /** Payload layouts, as stored. These numbers go on the wire and are never reordered. */
  public static final byte KIND_ARRAY = 0;

  public static final byte KIND_BITMAP = 1;
  public static final byte KIND_RUN = 2;

  /**
   * Rejects what a row can be rejected for on its own.
   *
   * <p>The payload is copied, so a caller reusing its array cannot change a chunk after the fact.
   */
  public ContainerChunk {
    if (kind != KIND_ARRAY && kind != KIND_BITMAP && kind != KIND_RUN) {
      throw new IllegalArgumentException("unknown container kind " + kind);
    }
    if (cardinality < 1 || cardinality > CHUNK_CARD) {
      throw new IllegalArgumentException(
          "a container holds 1 to " + CHUNK_CARD + " ordinals, not " + cardinality);
    }
    if (prefix48 >>> (Long.SIZE - CHUNK_BITS) != 0) {
      throw new IllegalArgumentException(
          "prefix48 " + UnsignedLongs.toString(prefix48) + " does not fit 48 bits");
    }
    payload = Objects.requireNonNull(payload, "payload").clone();
  }

  /** The payload bytes, copied. */
  @Override
  public byte[] payload() {
    return payload.clone();
  }

  /** The payload's length, without copying it. */
  public int payloadLength() {
    return payload.length;
  }

  /** Equal when every component is, the payload compared by content. */
  @Override
  public boolean equals(Object other) {
    return other instanceof ContainerChunk that
        && key == that.key
        && prefix48 == that.prefix48
        && kind == that.kind
        && cardinality == that.cardinality
        && Arrays.equals(payload, that.payload);
  }

  @Override
  public int hashCode() {
    return Objects.hash(key, prefix48, kind, cardinality, Arrays.hashCode(payload));
  }

  @Override
  public String toString() {
    return "ContainerChunk[key="
        + UnsignedLongs.toString(key)
        + ", prefix48="
        + UnsignedLongs.toString(prefix48)
        + ", kind="
        + kind
        + ", cardinality="
        + cardinality
        + ", payload="
        + payload.length
        + " bytes]";
  }

  /** Writes the payload into {@code target}, which the client's batch writer does per row. */
  void writePayload(org.apache.arrow.vector.VarBinaryVector target, int row) {
    target.setSafe(row, payload);
  }

  /**
   * The smallest valid container holding {@code values}, the low 16 bits of one chunk's ordinals.
   *
   * <p>{@code values} must be non-empty, ascending and unique, each in {@code [0, 65535]}. All
   * three are checked. The server's decoder would catch a descending array payload, but not a
   * caller who passed unsorted values and got a run encoding, because the runs derived from them
   * are structurally valid and describe a different set.
   *
   * <p>Smallest, rather than the engine's own choice. {@code yesno-core} picks a representation
   * with hysteresis -- a bitmap demotes only well below the cardinality at which an array would
   * first be smaller -- because it is deciding whether to re-encode a container under mutation, and
   * flapping costs more than either choice. A fresh encode has nothing to flap against, so the only
   * question is bytes on the wire, and the engine normalizes what it receives anyway. A tie goes to
   * the lowest kind.
   *
   * <p>This is an encoder of its own rather than a copy of {@link RoaringBitmap}'s containers. A
   * container payload is three fixed layouts over sorted 16-bit values, and going through {@code
   * org.roaringbitmap} instead would mean its one public container route, a serialized size two
   * bytes larger than an array payload, big-endian {@link ByteBuffer}s by default, and a 32-bit
   * bitmap that reaches only {@code prefix48 < 2^16}.
   */
  public static ContainerChunk encode(long key, long prefix48, int[] values) {
    Objects.requireNonNull(values, "values");
    if (values.length == 0) {
      throw new IllegalArgumentException("a container needs at least one value");
    }
    for (int index = 0; index < values.length; index++) {
      int value = values[index];
      if (value < 0 || value >= CHUNK_CARD) {
        throw new IllegalArgumentException(
            "chunk value " + value + " at index " + index + " is outside [0, " + CHUNK_CARD + ")");
      }
      if (index > 0 && value <= values[index - 1]) {
        throw new IllegalArgumentException(
            "chunk values must ascend and be unique: value "
                + value
                + " at index "
                + index
                + " follows "
                + values[index - 1]);
      }
    }

    int runs = runCount(values);
    // Every viable candidate, then the smallest; a later kind replaces the
    // incumbent only when strictly smaller, so a tie goes to the lowest kind.
    // An array drops out above ARRAY_MAX whatever its byte count says.
    byte kind = KIND_BITMAP;
    int size = BITMAP_BYTES;
    if (values.length <= ARRAY_MAX && 2 * values.length <= size) {
      kind = KIND_ARRAY;
      size = 2 * values.length;
    }
    if (2 + 4 * runs < size) {
      kind = KIND_RUN;
      size = 2 + 4 * runs;
    }

    byte[] payload =
        switch (kind) {
          case KIND_ARRAY -> arrayPayload(values);
          case KIND_BITMAP -> bitmapPayload(values);
          default -> runPayload(values, runs);
        };
    // Not tautological: each builder owns its own length expression, and this
    // is what catches one drifting from the size it was chosen by.
    if (payload.length != size) {
      throw new IllegalStateException(
          "kind " + kind + " encoded " + payload.length + " bytes but was chosen at " + size);
    }
    return new ContainerChunk(key, prefix48, kind, values.length, payload);
  }

  /**
   * Every chunk of one key's ordinals, in ascending chunk order.
   *
   * <p>Unlike {@link #encode}, which refuses unsorted values, the ordinals need not arrive sorted
   * and may repeat: a caller holding a whole set has no order to offer, while one holding a single
   * chunk's values is close enough to the wire that a silent sort would hide a bug. They are raw
   * unsigned 64-bit bits, and the maximum, {@code -1L}, is reserved and refused.
   */
  public static List<ContainerChunk> ofOrdinals(long key, long... ordinals) {
    Objects.requireNonNull(ordinals, "ordinals");
    long[] sorted = ordinals.clone();
    for (long ordinal : sorted) {
      if (ordinal == -1L) {
        throw new IllegalArgumentException(
            "the maximum unsigned 64-bit value is reserved and cannot be stored as an ordinal");
      }
    }
    // Unsigned order, by flipping the sign bit around a signed sort.
    for (int index = 0; index < sorted.length; index++) {
      sorted[index] ^= Long.MIN_VALUE;
    }
    Arrays.sort(sorted);
    for (int index = 0; index < sorted.length; index++) {
      sorted[index] ^= Long.MIN_VALUE;
    }

    List<ContainerChunk> chunks = new ArrayList<>();
    int[] values = new int[CHUNK_CARD];
    int index = 0;
    while (index < sorted.length) {
      long prefix = sorted[index] >>> CHUNK_BITS;
      int count = 0;
      while (index < sorted.length && sorted[index] >>> CHUNK_BITS == prefix) {
        int value = (int) (sorted[index] & (CHUNK_CARD - 1));
        if (count == 0 || values[count - 1] != value) {
          values[count++] = value;
        }
        index++;
      }
      chunks.add(encode(key, prefix, Arrays.copyOf(values, count)));
    }
    return chunks;
  }

  /**
   * Every chunk of a {@link RoaringBitmap}, read as unsigned 32-bit ordinals under {@code key}.
   *
   * <p>A 32-bit bitmap reaches only the first 65,536 chunks. Use {@link #ofOrdinals} for ordinals
   * above {@code 2^32 - 1}.
   */
  public static List<ContainerChunk> ofBitmap(long key, RoaringBitmap bitmap) {
    Objects.requireNonNull(bitmap, "bitmap");
    List<ContainerChunk> chunks = new ArrayList<>();
    int[] values = new int[CHUNK_CARD];
    int count = 0;
    int prefix = -1;
    IntIterator iterator = bitmap.getIntIterator();
    while (iterator.hasNext()) {
      int ordinal = iterator.next();
      int high = ordinal >>> CHUNK_BITS;
      if (high != prefix && count > 0) {
        chunks.add(encode(key, prefix, Arrays.copyOf(values, count)));
        count = 0;
      }
      prefix = high;
      values[count++] = ordinal & (CHUNK_CARD - 1);
    }
    if (count > 0) {
      chunks.add(encode(key, prefix, Arrays.copyOf(values, count)));
    }
    return chunks;
  }

  /** The number of maximal ascending intervals in {@code values}. */
  private static int runCount(int[] values) {
    int runs = 1;
    for (int index = 1; index < values.length; index++) {
      if (values[index] != values[index - 1] + 1) {
        runs++;
      }
    }
    return runs;
  }

  private static byte[] arrayPayload(int[] values) {
    ByteBuffer payload = ByteBuffer.allocate(2 * values.length).order(ByteOrder.LITTLE_ENDIAN);
    for (int value : values) {
      payload.putShort((short) value);
    }
    return payload.array();
  }

  private static byte[] bitmapPayload(int[] values) {
    // 1024 little-endian u64 words, bit i representing value i. Within a
    // little-endian word, byte b holds bits 8b..8b+8, so the payload is one
    // flat LSB-first bit array and the word size never appears here.
    byte[] payload = new byte[BITMAP_BYTES];
    for (int value : values) {
      payload[value >>> 3] |= (byte) (1 << (value & 7));
    }
    return payload;
  }

  /**
   * A run payload: the interval count, then {@code ( start, length - 1 )} per interval, all
   * little-endian {@code u16}.
   *
   * <p>Maximal intervals, not merely non-overlapping ones. The Roaring spec permits adjacent runs
   * and the server's decoder coalesces them, but only because a foreign file may carry them; the
   * engine's run kernels assume maximal intervals, so emitting adjacent ones would make the server
   * undo this method's output.
   */
  private static byte[] runPayload(int[] values, int runs) {
    ByteBuffer payload = ByteBuffer.allocate(2 + 4 * runs).order(ByteOrder.LITTLE_ENDIAN);
    payload.putShort((short) runs);
    int start = values[0];
    int previous = values[0];
    for (int index = 1; index < values.length; index++) {
      int value = values[index];
      if (value == previous + 1) {
        previous = value;
        continue;
      }
      payload.putShort((short) start).putShort((short) (previous - start));
      start = value;
      previous = value;
    }
    payload.putShort((short) start).putShort((short) (previous - start));
    return payload.array();
  }
}
