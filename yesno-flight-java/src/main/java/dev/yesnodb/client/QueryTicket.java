package dev.yesnodb.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.Objects;
import java.util.Optional;

/** The decoded yesnodb metadata inside an opaque Arrow Flight ticket. */
public final class QueryTicket {
  /**
   * The fixed ticket header length: six little-endian {@code uint64}s, in the order this class
   * declares them. A ticket may append an encoded expression after this header.
   *
   * <p>This was 40 bytes until the set representation was added to the header on 2026-09-30, and
   * this constant was not widened with it. Nothing here caught that: the only server these tests
   * talk to is a fake in {@code YesnoClientTest} that mints its ticket from this very constant, so
   * the test and the code were self-consistently wrong together. The fixed vector in {@code
   * QueryTicketTest} is a literal rather than a constant for that reason -- it is the one thing in
   * this module that a widening on the Rust side can make fail.
   */
  public static final int HEADER_LENGTH = 48;

  private final long version;
  private final long key;
  private final long prefixLo;
  private final long prefixHi;
  private final long expressionHash;
  private final SetWire wire;
  private final SetExpression expression;
  private final byte[] encoded;

  private QueryTicket(
      long version,
      long key,
      long prefixLo,
      long prefixHi,
      long expressionHash,
      SetWire wire,
      SetExpression expression,
      byte[] encoded) {
    this.version = version;
    this.key = key;
    this.prefixLo = prefixLo;
    this.prefixHi = prefixHi;
    this.expressionHash = expressionHash;
    this.wire = wire;
    this.expression = expression;
    this.encoded = encoded;
  }

  /**
   * Decode and validate a complete ticket.
   *
   * @param encoded the opaque ticket bytes a yesnodb server issued
   * @return the decoded ticket
   * @throws IllegalArgumentException if the ticket is short, inverted, asks for an unknown
   *     representation, or carries an expression that will not parse
   */
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
    SetWire wire = SetWire.fromCode(input.getLong());
    if (Long.compareUnsigned(prefixLo, prefixHi) > 0) {
      throw new IllegalArgumentException("yesnodb ticket has an inverted prefix range");
    }
    SetExpression expression =
        input.hasRemaining()
            ? SetExpression.decode(Arrays.copyOfRange(encoded, HEADER_LENGTH, encoded.length))
            : null;
    return new QueryTicket(
        version, key, prefixLo, prefixHi, expressionHash, wire, expression, encoded.clone());
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

  /**
   * The representation the row stream for this ticket will use.
   *
   * @return the set representation named in the header
   */
  public SetWire wire() {
    return wire;
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
