package dev.yesnodb.client;

import java.math.BigInteger;
import java.util.Objects;

/**
 * A single arbitrary-precision signed integer.
 *
 * <p>Distinct from {@link IntExpression} rather than a widening of it. An {@code IntExpression} is
 * a count or a position -- an unsigned 64-bit value whose cost is bounded by node count alone. This
 * sort's cost depends on how wide its values are, which is why it is the only sort with
 * {@link #MAX_VALUE_BITS} over it.
 */
public sealed interface BigExpression
    permits BigExpression.Literal,
        BigExpression.Widen,
        BigExpression.ReadUint,
        BigExpression.ReadInt,
        BigExpression.Negate,
        BigExpression.Add,
        BigExpression.Subtract,
        BigExpression.Multiply,
        BigExpression.Divide,
        BigExpression.Remainder,
        BigExpression.Truncate,
        BigExpression.Saturate,
        BigExpression.Fold,
        BigExpression.PowMod {

  /**
   * The widest value a big-integer node may denote.
   *
   * <p>Multiplication adds the operands' widths, so without this a small payload describes a value
   * no server should try to build.
   */
  long MAX_VALUE_BITS = 1L << 20;

  /**
   * Work a query may ask for, in limb operations.
   *
   * <p>The first bound that is not about size. Modular exponentiation returns a value no wider
   * than its modulus, so the width bound finds nothing wrong with a payload naming a computation
   * that would not finish.
   *
   * <p>An admission bound, not a cost model: every rule is a deliberate upper bound, which is
   * right for refusing the absurd and wrong for ranking plans.
   */
  long MAX_WORK = 1L << 28;

  /** An upper bound, in bits, on the value this expression can denote. */
  long widthBound();

  /** An upper bound on the limb operations evaluating this costs. */
  long workBound();

  /** Limbs a value of {@code bits} bits occupies. */
  static long limbsOf(long bits) {
    return (bits + 63) / 64;
  }

  /** A literal. */
  record Literal(BigInteger value) implements BigExpression {
    /** Construct a literal. */
    public Literal {
      Objects.requireNonNull(value, "value");
      if (value.bitLength() > MAX_VALUE_BITS) {
        throw new IllegalArgumentException("value is wider than " + MAX_VALUE_BITS + " bits");
      }
    }

    @Override
    public long widthBound() {
      return value.bitLength();
    }

    @Override
    public long workBound() {
      return 0;
    }
  }

  /** A count as an arbitrary-precision integer: the inclusion of the naturals. */
  record Widen(IntExpression input) implements BigExpression {
    /** Construct a widening. */
    public Widen {
      Objects.requireNonNull(input, "input");
    }

    @Override
    public long widthBound() {
      return 64;
    }

    @Override
    public long workBound() {
      return 1;
    }
  }

  /**
   * A set read as a magnitude, keeping its low {@code widthBits} bits.
   *
   * <p>A set <em>is</em> an integer: ordinal {@code j} carries the {@code 2^j} term. Reading
   * narrower than the value occupies is exactly {@code x mod 2^widthBits}.
   */
  record ReadUint(SetExpression input, int widthBits) implements BigExpression {
    /** Construct an unsigned read. */
    public ReadUint {
      Objects.requireNonNull(input, "input");
      checkWidth(widthBits);
    }

    @Override
    public long widthBound() {
      return Integer.toUnsignedLong(widthBits);
    }

    @Override
    public long workBound() {
      return limbsOf(Integer.toUnsignedLong(widthBits));
    }
  }

  /**
   * A set read in two's complement over {@code widthBits}.
   *
   * <p>A separate node rather than a flag on {@link ReadUint}: the same bits denote two different
   * numbers and nothing in them says which was meant, so an operation that sometimes did one and
   * sometimes the other would have no error path, only a well-formed wrong answer.
   */
  record ReadInt(SetExpression input, int widthBits) implements BigExpression {
    /** Construct a two's-complement read. */
    public ReadInt {
      Objects.requireNonNull(input, "input");
      checkWidth(widthBits);
    }

    @Override
    public long widthBound() {
      return Integer.toUnsignedLong(widthBits);
    }

    @Override
    public long workBound() {
      return limbsOf(Integer.toUnsignedLong(widthBits));
    }
  }

  /** The additive inverse. */
  record Negate(BigExpression input) implements BigExpression {
    /** Construct a negation. */
    public Negate {
      Objects.requireNonNull(input, "input");
    }

    @Override
    public long widthBound() {
      return input.widthBound();
    }

    @Override
    public long workBound() {
      return input.workBound() + limbsOf(input.widthBound());
    }
  }

  /** Sum. */
  record Add(BigExpression left, BigExpression right) implements BigExpression {
    /** Construct a sum. */
    public Add {
      checkPair(left, right);
    }

    @Override
    public long widthBound() {
      return Math.max(left.widthBound(), right.widthBound()) + 1;
    }

    @Override
    public long workBound() {
      return left.workBound()
          + right.workBound()
          + Math.max(limbsOf(left.widthBound()), limbsOf(right.widthBound()));
    }
  }

  /** Difference. Total, which is the operation the signed sort exists for. */
  record Subtract(BigExpression left, BigExpression right) implements BigExpression {
    /** Construct a difference. */
    public Subtract {
      checkPair(left, right);
    }

    @Override
    public long widthBound() {
      return Math.max(left.widthBound(), right.widthBound()) + 1;
    }

    @Override
    public long workBound() {
      return left.workBound()
          + right.workBound()
          + Math.max(limbsOf(left.widthBound()), limbsOf(right.widthBound()));
    }
  }

  /** Product. The node that makes the width bound necessary: it adds the operands' widths. */
  record Multiply(BigExpression left, BigExpression right) implements BigExpression {
    /** Construct a product. */
    public Multiply {
      checkPair(left, right);
      if (left.widthBound() + right.widthBound() > MAX_VALUE_BITS) {
        throw new IllegalArgumentException("value is wider than " + MAX_VALUE_BITS + " bits");
      }
    }

    @Override
    public long widthBound() {
      return left.widthBound() + right.widthBound();
    }

    @Override
    public long workBound() {
      return left.workBound()
          + right.workBound()
          + limbsOf(left.widthBound()) * limbsOf(right.widthBound());
    }
  }

  /** Quotient, truncating toward zero. A zero divisor is a server error. */
  record Divide(BigExpression left, BigExpression right) implements BigExpression {
    /** Construct a quotient. */
    public Divide {
      checkPair(left, right);
    }

    @Override
    public long widthBound() {
      return left.widthBound();
    }

    @Override
    public long workBound() {
      return left.workBound()
          + right.workBound()
          + limbsOf(left.widthBound()) * limbsOf(right.widthBound());
    }
  }

  /** Remainder, carrying the sign of the dividend. */
  record Remainder(BigExpression left, BigExpression right) implements BigExpression {
    /** Construct a remainder. */
    public Remainder {
      checkPair(left, right);
    }

    @Override
    public long widthBound() {
      return Math.min(left.widthBound(), right.widthBound());
    }

    @Override
    public long workBound() {
      return left.workBound()
          + right.workBound()
          + limbsOf(left.widthBound()) * limbsOf(right.widthBound());
    }
  }

  /** Wrap into a {@code bits}-wide two's-complement field. */
  record Truncate(BigExpression input, int bits) implements BigExpression {
    /** Construct a wrap. */
    public Truncate {
      Objects.requireNonNull(input, "input");
      checkBits(bits);
    }

    @Override
    public long widthBound() {
      return Math.min(input.widthBound(), Integer.toUnsignedLong(bits));
    }

    @Override
    public long workBound() {
      return input.workBound() + limbsOf(input.widthBound());
    }
  }

  /**
   * Clamp into the same field {@link Truncate} wraps into.
   *
   * <p>One field, two overflow rules; they coincide only where nothing overflowed.
   */
  record Saturate(BigExpression input, int bits) implements BigExpression {
    /** Construct a clamp. */
    public Saturate {
      Objects.requireNonNull(input, "input");
      checkBits(bits);
    }

    @Override
    public long widthBound() {
      return Math.min(input.widthBound(), Integer.toUnsignedLong(bits));
    }

    @Override
    public long workBound() {
      return input.workBound() + limbsOf(input.widthBound());
    }
  }

  /**
   * Reduce a vector of big integers to one.
   *
   * <p>The transpose of {@link VecBigExpression.MapBig}: a map keeps one value per constituent, a
   * fold collapses them.
   */
  record Fold(VecBigExpression vector, BigFoldOp op) implements BigExpression {
    /** Construct a fold. */
    public Fold {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(op, "op");
      if (widthBoundOf(vector, op) > MAX_VALUE_BITS) {
        throw new IllegalArgumentException("value is wider than " + MAX_VALUE_BITS + " bits");
      }
    }

    @Override
    public long widthBound() {
      return widthBoundOf(vector, op);
    }

    @Override
    public long workBound() {
      long arity = vector.arity();
      long element = limbsOf(vector.elementBound());
      long reduce = arity * element;
      if (op == BigFoldOp.MUL) {
        // The accumulator grows as it goes, so the last multiply is against
        // the whole product.
        reduce = reduce * reduce;
      }
      return vector.workBound() + reduce;
    }

    /**
     * Only {@code MUL} grows with the arity.
     *
     * <p>A sum of {@code n} values below {@code 2^w} is below {@code 2^(w + bits(n))}, so addition
     * costs a handful of bits. A product reaches {@code n * w} -- the same unbounded product the
     * result bound exists for, except that here it lands in a single value.
     */
    private static long widthBoundOf(VecBigExpression vector, BigFoldOp op) {
      long arity = vector.arity();
      long widest = vector.elementBound();
      return switch (op) {
        case ADD -> widest + (64 - Long.numberOfLeadingZeros(arity));
        case MUL -> widest * arity;
        case MIN, MAX -> widest;
      };
    }
  }

  /**
   * {@code base^exp mod modulus}, by Barrett reduction.
   *
   * <p>The node {@link #MAX_WORK} exists for: its result is only as wide as the modulus, so the
   * width bound finds nothing wrong with it, while its cost grows with the exponent's bit count
   * times the square of the modulus's.
   *
   * <p>A zero modulus has no residues and a negative exponent is a modular inverse, which the
   * engine does not compute; both are server-side errors. A negative base enters its residue class
   * first, so the answer is always in {@code [0, modulus)}.
   */
  record PowMod(BigExpression base, BigExpression exp, BigExpression modulus)
      implements BigExpression {
    /** Construct a modular exponentiation. */
    public PowMod {
      Objects.requireNonNull(base, "base");
      Objects.requireNonNull(exp, "exp");
      Objects.requireNonNull(modulus, "modulus");
    }

    @Override
    public long widthBound() {
      // A residue is bounded by its modulus and by nothing else.
      return modulus.widthBound();
    }

    @Override
    public long workBound() {
      long mLimbs = limbsOf(modulus.widthBound());
      // One squaring per exponent bit and one multiply per set bit, each
      // Barrett-reduced; the reduction is two multiplies.
      long step = 4 * mLimbs * mLimbs;
      return base.workBound()
          + exp.workBound()
          + modulus.workBound()
          + exp.widthBound() * step;
    }
  }

  /**
   * A literal from a signed machine integer.
   *
   * <p>The scalar half of the query language's {@code big( .. )}. The vector half is
   * {@link VecBigExpression#of(long...)}, since a record cannot be overloaded on shape.
   */
  static BigExpression of(long value) {
    return new Literal(BigInteger.valueOf(value));
  }

  private static void checkWidth(int widthBits) {
    long width = Integer.toUnsignedLong(widthBits);
    if (width == 0) {
      throw new IllegalArgumentException("a big-integer read has zero width");
    }
    if (width > MAX_VALUE_BITS) {
      throw new IllegalArgumentException("value is wider than " + MAX_VALUE_BITS + " bits");
    }
  }

  private static void checkBits(int bits) {
    if (Integer.toUnsignedLong(bits) > MAX_VALUE_BITS) {
      throw new IllegalArgumentException("value is wider than " + MAX_VALUE_BITS + " bits");
    }
  }

  private static void checkPair(BigExpression left, BigExpression right) {
    Objects.requireNonNull(left, "left");
    Objects.requireNonNull(right, "right");
  }
}
