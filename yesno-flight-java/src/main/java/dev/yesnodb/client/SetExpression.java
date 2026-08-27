package dev.yesnodb.client;

import java.util.Arrays;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/**
 * A set expression in yesnodb's dependency-free v1 wire format.
 *
 * <p>All {@code long} key, ordinal, and stride values carry raw unsigned 64-bit bits. Use {@link
 * UnsignedLongs} at text or {@code BigInteger} boundaries.
 */
public sealed interface SetExpression
    permits SetExpression.Empty,
        SetExpression.Key,
        SetExpression.Range,
        SetExpression.Literal,
        SetExpression.And,
        SetExpression.Or,
        SetExpression.AndNot,
        SetExpression.ViewSelect,
        SetExpression.ViewFold,
        SetExpression.ViewExpand {
  /** Maximum expression nesting accepted by the server. */
  int MAX_DEPTH = 32;

  /** Maximum expression nodes accepted by the server. */
  int MAX_NODES = 4096;

  /** The empty set. */
  record Empty() implements SetExpression {}

  /** A stored posting list. */
  record Key(long key) implements SetExpression {}

  /** A half-open ordinal range {@code [lo, hi)}. */
  record Range(long lo, long hi) implements SetExpression {}

  /** A canonical materialized set of ordinals. */
  record Literal(List<Long> ordinals) implements SetExpression {
    /** Normalize unsigned member order, remove duplicates, and enforce the ordinal ceiling. */
    public Literal {
      Objects.requireNonNull(ordinals, "ordinals");
      List<Long> canonical = new ArrayList<>(ordinals.size());
      for (Long ordinal : ordinals) {
        Objects.requireNonNull(ordinal, "literal ordinal");
        if (ordinal == -1L) {
          throw new IllegalArgumentException(
              "18446744073709551615 is outside the ordinal universe");
        }
        canonical.add(ordinal);
      }
      canonical.sort(Long::compareUnsigned);
      for (int index = canonical.size() - 1; index > 0; index--) {
        if (canonical.get(index).equals(canonical.get(index - 1))) {
          canonical.remove(index);
        }
      }
      ordinals = List.copyOf(canonical);
    }
  }

  /** Intersection of one or more operands. */
  record And(List<SetExpression> operands) implements SetExpression {
    /** Construct an intersection and take an immutable copy of its operands. */
    public And {
      operands = checkedOperands(operands);
    }
  }

  /** Union of one or more operands. */
  record Or(List<SetExpression> operands) implements SetExpression {
    /** Construct a union and take an immutable copy of its operands. */
    public Or {
      operands = checkedOperands(operands);
    }
  }

  /** Set difference {@code include - exclude}. */
  record AndNot(SetExpression include, SetExpression exclude) implements SetExpression {
    /** Construct a set difference. */
    public AndNot {
      Objects.requireNonNull(include, "include");
      Objects.requireNonNull(exclude, "exclude");
    }
  }

  /** Select one logical constituent from a packed key. */
  record ViewSelect(long key, ViewSpec view, long set) implements SetExpression {
    /** Construct a packed-view selection. */
    public ViewSelect {
      Objects.requireNonNull(view, "view");
      if (set < 0 || set > 0xffff_ffffL || set >= view.sets()) {
        throw new IllegalArgumentException("set is outside the packed-view descriptor");
      }
    }
  }

  /** Reduce every constituent of a packed key. */
  record ViewFold(long key, ViewSpec view, ViewReduce reduce) implements SetExpression {
    /** Construct a packed-view reduction. */
    public ViewFold {
      Objects.requireNonNull(view, "view");
      Objects.requireNonNull(reduce, "reduce");
    }
  }

  /** Expand logical ordinals into every physical constituent slot. */
  record ViewExpand(SetExpression input, ViewSpec view) implements SetExpression {
    /** Construct a packed-view expansion. */
    public ViewExpand {
      Objects.requireNonNull(input, "input");
      Objects.requireNonNull(view, "view");
    }
  }

  /** Construct the empty set. */
  static SetExpression empty() {
    return new Empty();
  }

  /** Construct a key lookup from raw unsigned 64-bit bits. */
  static SetExpression key(long key) {
    return new Key(key);
  }

  /** Construct a half-open range from raw unsigned 64-bit bits. */
  static SetExpression range(long lo, long hi) {
    return new Range(lo, hi);
  }

  /** Construct a canonical materialized ordinal set from raw unsigned bits. */
  static SetExpression literal(long... ordinals) {
    return new Literal(Arrays.stream(ordinals).boxed().toList());
  }

  /** Construct an intersection. */
  static SetExpression and(SetExpression... operands) {
    return new And(Arrays.asList(operands));
  }

  /** Construct a union. */
  static SetExpression or(SetExpression... operands) {
    return new Or(Arrays.asList(operands));
  }

  /** Construct a set difference. */
  static SetExpression andNot(SetExpression include, SetExpression exclude) {
    return new AndNot(include, exclude);
  }

  /**
   * Construct symmetric difference using only v1 nodes, preserving compatibility with every v1
   * server.
   */
  static SetExpression xor(SetExpression left, SetExpression right) {
    Objects.requireNonNull(left, "left");
    Objects.requireNonNull(right, "right");
    return andNot(or(left, right), and(left, right));
  }

  /** Construct complement over {@code [0, 2^64 - 1)}, excluding the reserved maximum ordinal. */
  static SetExpression complement(SetExpression expression) {
    return andNot(range(0, -1L), expression);
  }

  /** Encode this expression, including the {@code YSNX} v1 header. */
  default byte[] encode() {
    return SetExpressionCodec.encode(this);
  }

  /** Decode and validate one complete v1 expression. */
  static SetExpression decode(byte[] encoded) {
    Objects.requireNonNull(encoded, "encoded");
    return SetExpressionCodec.decode(encoded);
  }

  private static List<SetExpression> checkedOperands(List<SetExpression> operands) {
    Objects.requireNonNull(operands, "operands");
    if (operands.isEmpty()) {
      throw new IllegalArgumentException("AND/OR must have at least one operand");
    }
    return List.copyOf(operands);
  }
}
