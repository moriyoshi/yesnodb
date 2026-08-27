package dev.yesnodb.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.Objects;
import java.util.Optional;

/** The decoded yesnodb metadata inside an opaque Arrow Flight ticket. */
public final class QueryTicket {
  /** The fixed v1 ticket header length. */
  public static final int HEADER_LENGTH = 40;

  private final long version;
  private final long key;
  private final long prefixLo;
  private final long prefixHi;
  private final long expressionHash;
  private final SetExpression expression;
  private final byte[] encoded;

  private QueryTicket(
      long version,
      long key,
      long prefixLo,
      long prefixHi,
      long expressionHash,
      SetExpression expression,
      byte[] encoded) {
    this.version = version;
    this.key = key;
    this.prefixLo = prefixLo;
    this.prefixHi = prefixHi;
    this.expressionHash = expressionHash;
    this.expression = expression;
    this.encoded = encoded;
  }

  /** Decode and validate a complete v1 ticket. */
  public static QueryTicket decode(byte[] encoded) {
    Objects.requireNonNull(encoded, "encoded");
    if (encoded.length < HEADER_LENGTH) {
      throw new IllegalArgumentException(
          "yesnodb ticket has " + encoded.length + " bytes; expected at least " + HEADER_LENGTH);
    }
    ByteBuffer input = ByteBuffer.wrap(encoded).order(ByteOrder.LITTLE_ENDIAN);
    long version = input.getLong();
    long key = input.getLong();
    long prefixLo = input.getLong();
    long prefixHi = input.getLong();
    long expressionHash = input.getLong();
    if (Long.compareUnsigned(prefixLo, prefixHi) > 0) {
      throw new IllegalArgumentException("yesnodb ticket has an inverted prefix range");
    }
    SetExpression expression =
        input.hasRemaining()
            ? SetExpression.decode(Arrays.copyOfRange(encoded, HEADER_LENGTH, encoded.length))
            : null;
    return new QueryTicket(
        version, key, prefixLo, prefixHi, expressionHash, expression, encoded.clone());
  }

  /** The database snapshot version shared by the count and row stream. */
  public long version() {
    return version;
  }

  /** The primary key, as raw unsigned 64-bit bits. */
  public long key() {
    return key;
  }

  /** The inclusive low 48-bit prefix bound. */
  public long prefixLo() {
    return prefixLo;
  }

  /** The exclusive high 48-bit prefix bound. */
  public long prefixHi() {
    return prefixHi;
  }

  /** The reserved expression identity field. */
  public long expressionHash() {
    return expressionHash;
  }

  /** The pushed-down expression, if this is an expression query. */
  public Optional<SetExpression> expression() {
    return Optional.ofNullable(expression);
  }

  /** Return a defensive copy of the opaque ticket bytes. */
  public byte[] encoded() {
    return encoded.clone();
  }
}
