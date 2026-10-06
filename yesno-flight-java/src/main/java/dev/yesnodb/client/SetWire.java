package dev.yesnodb.client;

/**
 * How a set result is encoded in the {@code DoGet} stream.
 *
 * <p>Only sets have a choice: a vector or a scalar answers one small batch whose shape no
 * representation question applies to. The codes go on the wire and must never be reordered.
 */
public enum SetWire {
  /**
   * One {@code uint64} per ordinal. The default, because a client that has not asked for containers
   * may have no decoder for them.
   */
  ORDINALS(0),
  /** Container payloads, byte-identical to what the page store and a {@code .roaring} file hold. */
  CONTAINERS(1),
  /**
   * A wholly materialized bitvector over the ticket's prefix window, one bit per ordinal position,
   * gaps included.
   */
  BITVECTOR(2);

  private final long code;

  SetWire(long code) {
    this.code = code;
  }

  /**
   * The value this representation takes on the wire.
   *
   * @return the wire code
   */
  public long code() {
    return code;
  }

  /**
   * Resolve a wire code.
   *
   * <p>An unrecognised representation is an error, never a fallback to the default: reading a stream
   * in a representation the server did not promise is corruption, and indistinguishable from a
   * server that understood.
   *
   * @param code the value read from a ticket header
   * @return the representation that code names
   * @throws IllegalArgumentException if no representation has that code
   */
  public static SetWire fromCode(long code) {
    for (SetWire wire : values()) {
      if (wire.code == code) {
        return wire;
      }
    }
    throw new IllegalArgumentException("yesnodb ticket asks for set representation " + code);
  }
}
