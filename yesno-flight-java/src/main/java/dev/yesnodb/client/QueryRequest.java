package dev.yesnodb.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Objects;
import java.util.OptionalLong;

/** A remotely executable expression and the exact snapshot version it optionally requires. */
public record QueryRequest(SetExpression expression, OptionalLong version) {
  private static final byte[] MAGIC = "YSNQ".getBytes(StandardCharsets.US_ASCII);
  private static final int HEADER_LENGTH = 14;
  private static final int PINNED = 1;

  /** Validate a query request and retain its immutable values. */
  public QueryRequest {
    expression = Objects.requireNonNull(expression, "expression");
    version = Objects.requireNonNull(version, "version");
  }

  /** Construct a request evaluated at the server's current snapshot. */
  public static QueryRequest current(SetExpression expression) {
    return new QueryRequest(expression, OptionalLong.empty());
  }

  /** Construct a strict request evaluated at exactly {@code version}. */
  public static QueryRequest at(SetExpression expression, long version) {
    return new QueryRequest(expression, OptionalLong.of(version));
  }

  /** Encode this request as one complete {@code YSNQ} v1 descriptor command. */
  public byte[] encode() {
    byte[] encodedExpression = expression.encode();
    ByteBuffer buffer =
        ByteBuffer.allocate(HEADER_LENGTH + encodedExpression.length)
            .order(ByteOrder.LITTLE_ENDIAN);
    buffer.put(MAGIC);
    buffer.put((byte) 1);
    buffer.put((byte) (version.isPresent() ? PINNED : 0));
    buffer.putLong(version.orElse(0));
    buffer.put(encodedExpression);
    return buffer.array();
  }

  /** Decode and validate one complete {@code YSNQ} v1 descriptor command. */
  public static QueryRequest decode(byte[] encoded) {
    Objects.requireNonNull(encoded, "encoded");
    if (encoded.length < HEADER_LENGTH) {
      throw new IllegalArgumentException("query request ended in its header");
    }
    ByteBuffer buffer = ByteBuffer.wrap(encoded).order(ByteOrder.LITTLE_ENDIAN);
    byte[] magic = new byte[MAGIC.length];
    buffer.get(magic);
    if (!Arrays.equals(magic, MAGIC)) {
      throw new IllegalArgumentException("not a yesnodb query request");
    }
    int wireVersion = Byte.toUnsignedInt(buffer.get());
    int flags = Byte.toUnsignedInt(buffer.get());
    if (wireVersion != 1 || (flags & ~PINNED) != 0) {
      throw new IllegalArgumentException("unsupported query request version " + wireVersion);
    }
    long rawVersion = buffer.getLong();
    boolean pinned = (flags & PINNED) != 0;
    if (!pinned && rawVersion != 0) {
      throw new IllegalArgumentException("an unpinned query request has a non-zero version");
    }
    byte[] expression = new byte[buffer.remaining()];
    buffer.get(expression);
    SetExpression decoded = SetExpression.decode(expression);
    return pinned ? at(decoded, rawVersion) : current(decoded);
  }

  /** Whether bytes have the complete header and magic of a query request. */
  public static boolean looksLikeRequest(byte[] encoded) {
    Objects.requireNonNull(encoded, "encoded");
    return encoded.length >= HEADER_LENGTH && Arrays.equals(encoded, 0, 4, MAGIC, 0, 4);
  }
}
