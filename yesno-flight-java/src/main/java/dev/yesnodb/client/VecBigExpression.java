package dev.yesnodb.client;

import java.util.List;
import java.util.Objects;

/**
 * One arbitrary-precision integer per constituent.
 *
 * <p>There is no unsigned counterpart, deliberately: sign is a property of the <em>reading</em> --
 * {@link BigExpression.ReadUint} yields a magnitude, {@link BigExpression.ReadInt} a two's
 * complement of the same bits -- so a vector of unsigned values would be a sort whose only content
 * is a promise the element's own node already makes.
 */
public sealed interface VecBigExpression
    permits VecBigExpression.Listing,
        VecBigExpression.MapBig,
        VecBigExpression.Zip,
        VecBigExpression.Scale {

  /**
   * The widest whole result such a vector may denote.
   *
   * <p>The product of two bounded factors is not bounded by either: 4096 constituents of 2^20 bits
   * each is half a gigabyte of answer from a payload of a few dozen bytes.
   */
  long MAX_RESULT_BITS = 1L << 24;

  /** How many elements this vector has. */
  int arity();

  /** An upper bound, in bits, on any one element. */
  long elementBound();

  /** An upper bound on the limb operations this vector costs. */
  long workBound();

  /** An upper bound, in bits, on the whole result. */
  default long resultBound() {
    return Integer.toUnsignedLong(arity()) * elementBound();
  }

  /** A literal vector of big integers. */
  record Listing(List<BigExpression> elements) implements VecBigExpression {
    /** Construct a literal vector and take an immutable copy of its elements. */
    public Listing {
      Objects.requireNonNull(elements, "elements");
      if (elements.isEmpty()) {
        throw new IllegalArgumentException("a vector must have at least one element");
      }
      elements = List.copyOf(elements);
      checkResult(elements.size(), widest(elements));
    }

    @Override
    public int arity() {
      return elements.size();
    }

    @Override
    public long elementBound() {
      return widest(elements);
    }

    @Override
    public long workBound() {
      long total = 0;
      for (BigExpression e : elements) {
        total += e.workBound();
      }
      return total;
    }

    private static long widest(List<BigExpression> elements) {
      long widest = 0;
      for (BigExpression e : elements) {
        widest = Math.max(widest, e.widthBound());
      }
      return widest;
    }
  }

  /** A big-integer query applied to every element of a vector of sets. */
  record MapBig(VecSetExpression vector, BigExpression body) implements VecBigExpression {
    /** Construct a map. */
    public MapBig {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(body, "body");
      checkResult(vector.arity(), body.widthBound());
    }

    @Override
    public int arity() {
      return vector.arity();
    }

    @Override
    public long elementBound() {
      return body.widthBound();
    }

    /** The body runs once per constituent. */
    @Override
    public long workBound() {
      return Integer.toUnsignedLong(arity()) * body.workBound();
    }
  }

  /**
   * Combine two vectors of big integers position by position.
   *
   * <p>Both arities are statically known, so a mismatch is refused when the node is built rather
   * than discovered while evaluating -- which is what lets the evaluator's own zip be total.
   */
  record Zip(VecBigExpression left, VecBigExpression right, BigBinOp op)
      implements VecBigExpression {
    /** Construct a zip, refusing operands of unequal arity. */
    public Zip {
      Objects.requireNonNull(left, "left");
      Objects.requireNonNull(right, "right");
      Objects.requireNonNull(op, "op");
      if (left.arity() != right.arity()) {
        throw new IllegalArgumentException(
            "zip needs equal arities, not " + left.arity() + " and " + right.arity());
      }
      checkResult(left.arity(), op.widthBound(left.elementBound(), right.elementBound()));
    }

    @Override
    public int arity() {
      return left.arity();
    }

    @Override
    public long elementBound() {
      return op.widthBound(left.elementBound(), right.elementBound());
    }

    /** One operation per position, plus whatever the operands cost. */
    @Override
    public long workBound() {
      long per = op.elementWork(left.elementBound(), right.elementBound());
      return left.workBound() + right.workBound() + Integer.toUnsignedLong(arity()) * per;
    }
  }

  /** Combine every element of a vector with one scalar. */
  record Scale(VecBigExpression vector, BigExpression scalar, BigBinOp op)
      implements VecBigExpression {
    /** Construct a scale. */
    public Scale {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(scalar, "scalar");
      Objects.requireNonNull(op, "op");
      checkResult(vector.arity(), op.widthBound(vector.elementBound(), scalar.widthBound()));
    }

    @Override
    public int arity() {
      return vector.arity();
    }

    @Override
    public long elementBound() {
      return op.widthBound(vector.elementBound(), scalar.widthBound());
    }

    /** One operation per position, plus whatever the operands cost. */
    @Override
    public long workBound() {
      long per = op.elementWork(vector.elementBound(), scalar.widthBound());
      return vector.workBound() + scalar.workBound() + Integer.toUnsignedLong(arity()) * per;
    }
  }

  /**
   * A literal vector from plain numbers.
   *
   * <p>The query language spells this {@code big( [ 1, 2, 3 ] )}, and the marker is not
   * decoration: a bracketed list is a vector of <em>sets</em> everywhere, so a vector of big
   * integers has to say so where it is written.
   */
  static VecBigExpression of(long... values) {
    List<BigExpression> elements = new java.util.ArrayList<>(values.length);
    for (long v : values) {
      elements.add(BigExpression.of(v));
    }
    return new Listing(elements);
  }

  private static void checkResult(int arity, long elementBound) {
    if (Integer.toUnsignedLong(arity) * elementBound > MAX_RESULT_BITS) {
      throw new IllegalArgumentException("result is larger than " + MAX_RESULT_BITS + " bits");
    }
  }
}
