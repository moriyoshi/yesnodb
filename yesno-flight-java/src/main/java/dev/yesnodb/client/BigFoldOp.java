package dev.yesnodb.client;

/**
 * How {@link BigExpression.Fold} combines a vector's elements.
 *
 * <p>Four, where the set fold has three: the carrier is the integers, where {@code +} and {@code *}
 * are the ring operations and {@code min} / {@code max} the lattice ones. All four are associative
 * and commutative, so the answer does not depend on the order constituents are visited in. None
 * needs an identity, because a vector is never empty -- which is what admits {@code min} and
 * {@code max} in an unbounded domain.
 */
public enum BigFoldOp {
  /** Sum. */
  ADD((byte) 0),
  /** Product. The one whose width grows with the arity. */
  MUL((byte) 1),
  /** Least element. */
  MIN((byte) 2),
  /** Greatest element. */
  MAX((byte) 3);

  private final byte wire;

  BigFoldOp(byte wire) {
    this.wire = wire;
  }

  /** The byte this operator encodes as. */
  public byte wire() {
    return wire;
  }

  /** The operator a byte denotes, or null if none does. */
  public static BigFoldOp fromWire(byte b) {
    for (BigFoldOp op : values()) {
      if (op.wire == b) {
        return op;
      }
    }
    return null;
  }
}
