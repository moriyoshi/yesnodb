package dev.yesnodb.client;

import java.util.Objects;

/** A single count or position. */
public sealed interface IntExpression
    permits IntExpression.Literal,
        IntExpression.Cardinality,
        IntExpression.Rank,
        IntExpression.At {

  /** A literal, carrying raw unsigned 64-bit bits. */
  record Literal(long value) implements IntExpression {}

  /** How many ordinals a set holds. */
  record Cardinality(SetExpression input) implements IntExpression {
    /** Construct a cardinality query. */
    public Cardinality {
      Objects.requireNonNull(input, "input");
    }
  }

  /** How many of a set's ordinals are strictly below a position. */
  record Rank(SetExpression input, long position) implements IntExpression {
    /** Construct a rank query. */
    public Rank {
      Objects.requireNonNull(input, "input");
    }
  }

  /** One element of a vector of integers, by zero-based index. */
  record At(VecIntExpression vector, long index) implements IntExpression {
    /** Construct an index into a vector of integers. */
    public At {
      Objects.requireNonNull(vector, "vector");
      if (index < 0 || index > 0xffff_ffffL || index >= vector.arity()) {
        throw new IllegalArgumentException("index is at or above the vector's arity");
      }
    }
  }

  /** Construct a literal. */
  static IntExpression literal(long value) {
    return new Literal(value);
  }

  /** Construct a cardinality query. */
  static IntExpression cardinality(SetExpression input) {
    return new Cardinality(input);
  }

  /** Construct a rank query. */
  static IntExpression rank(SetExpression input, long position) {
    return new Rank(input, position);
  }

  /** Construct an index into a vector of integers. */
  static IntExpression at(VecIntExpression vector, long index) {
    return new At(vector, index);
  }
}
