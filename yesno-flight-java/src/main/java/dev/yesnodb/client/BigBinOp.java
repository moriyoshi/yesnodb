package dev.yesnodb.client;

/**
 * How {@link VecBigExpression.Zip} and {@link VecBigExpression.Scale} combine two big integers.
 *
 * <p>The same five operations the scalar nodes offer, bounded by the same rules -- so a zip and the
 * equivalent written out element by element are bounded identically. That is the point of sharing
 * the rules rather than restating them: a vector operation that was bounded more loosely than its
 * own expansion would be a way around the width budget.
 */
public enum BigBinOp {
  /** Sum. */
  ADD((byte) 0),
  /** Difference. */
  SUB((byte) 1),
  /** Product. The one whose width grows, here per element. */
  MUL((byte) 2),
  /** Quotient, truncating toward zero. A zero divisor is an evaluation error. */
  DIV((byte) 3),
  /** Remainder, carrying the sign of the dividend. */
  REM((byte) 4);

  private final byte wire;

  BigBinOp(byte wire) {
    this.wire = wire;
  }

  /** The byte this operator encodes as. */
  public byte wire() {
    return wire;
  }

  /** The operator a byte denotes, or null if none does. */
  public static BigBinOp fromWire(byte b) {
    for (BigBinOp op : values()) {
      if (op.wire == b) {
        return op;
      }
    }
    return null;
  }

  /** The width one result can reach, from its operands' widths. */
  public long widthBound(long a, long b) {
    return switch (this) {
      case ADD, SUB -> Math.max(a, b) + 1;
      case MUL -> a + b;
      case DIV -> a;
      case REM -> Math.min(a, b);
    };
  }

  /**
   * The limb operations one element costs.
   *
   * <p>Additive operations are linear in the wider operand, multiplicative ones are the product of
   * both.
   */
  public long elementWork(long a, long b) {
    long la = (a + 63) / 64;
    long lb = (b + 63) / 64;
    return switch (this) {
      case ADD, SUB -> Math.max(la, lb);
      case MUL, DIV, REM -> la * lb;
    };
  }
}
