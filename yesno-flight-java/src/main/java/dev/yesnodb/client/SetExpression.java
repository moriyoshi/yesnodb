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
        SetExpression.At,
        SetExpression.Fold,
        SetExpression.Pack,
        SetExpression.Expand,
        SetExpression.Hole,
        SetExpression.Select,
        SetExpression.MapBool {
  /** Maximum expression nesting accepted by the server. */
  int MAX_DEPTH = 32;

  /** Maximum expression nodes accepted by the server. */
  int MAX_NODES = 4096;

  /**
   * Maximum constituents one view descriptor may pack.
   *
   * <p>Neither of the bounds above sees this: a descriptor is a fixed 13 bytes whatever it
   * declares, while the server loops over every constituent.
   */
  int MAX_VIEW_SETS = 4096;

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

  /**
   * One element of a vector, by zero-based index -- {@code v[ i ]}.
   *
   * <p>Total on a well-formed expression: a vector's arity is statically known, so an out-of-range
   * index is refused here rather than discovered by the server.
   */
  record At(VecSetExpression vector, long index) implements SetExpression {
    /** Construct an index into a vector. */
    public At {
      Objects.requireNonNull(vector, "vector");
      if (index < 0 || index > 0xffff_ffffL || index >= vector.arity()) {
        throw new IllegalArgumentException("index is at or above the vector's arity");
      }
    }
  }

  /** Combine every element of a vector into one set. */
  record Fold(VecSetExpression vector, FoldOp op) implements SetExpression {
    /** Construct a fold. */
    public Fold {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(op, "op");
    }
  }

  /** Pack a vector's elements into one set -- the uncurry direction. */
  record Pack(VecSetExpression vector, ViewSpec view) implements SetExpression {
    /** Construct a pack, checking the vector's arity against the descriptor. */
    public Pack {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(view, "view");
      if (vector.arity() != view.sets()) {
        throw new IllegalArgumentException(
            "packing " + vector.arity() + " sets under a " + view.sets() + "-set descriptor");
      }
    }
  }

  /** Expand logical ordinals into every physical constituent slot. */
  record Expand(SetExpression input, ViewSpec view) implements SetExpression {
    /** Construct a packed-view expansion. */
    public Expand {
      Objects.requireNonNull(input, "input");
      Objects.requireNonNull(view, "view");
    }
  }

  /**
   * {@code _} -- the element of the enclosing {@link VecSetExpression.MapSet} body.
   *
   * <p>The language has no variables, so this is a hole rather than a name: no binder, no scope,
   * no closure. Every {@code _} in one body denotes the same element.
   */
  record Hole() implements SetExpression {}

  /**
   * The {@code n}-th smallest ordinal, as a singleton -- or empty if there is none.
   *
   * <p>Yields a set rather than an integer because it is genuinely partial: no type knows a set's
   * cardinality, and an empty set says so without a reserved value a caller could forget to check.
   */
  record Select(SetExpression input, long index) implements SetExpression {
    /** Construct a positional selection. */
    public Select {
      Objects.requireNonNull(input, "input");
    }
  }

  /**
   * Which constituents satisfy a boolean body.
   *
   * <p>One truth value per constituent is a subset of the constituent indices, so this denotes a
   * set rather than a vector sort.
   */
  record MapBool(VecSetExpression vector, BoolExpression body) implements SetExpression {
    /** Construct a boolean map. */
    public MapBool {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(body, "body");
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

  /** Index into a vector by zero-based position. */
  static SetExpression at(VecSetExpression vector, long index) {
    return new At(vector, index);
  }

  /** Combine every element of a vector. */
  static SetExpression fold(VecSetExpression vector, FoldOp op) {
    return new Fold(vector, op);
  }

  /** Pack a vector's elements into one set. */
  static SetExpression pack(VecSetExpression vector, ViewSpec view) {
    return new Pack(vector, view);
  }

  /** Expand logical ordinals into every constituent slot. */
  static SetExpression expand(SetExpression input, ViewSpec view) {
    return new Expand(input, view);
  }

  /** Construct the hole, valid only inside a map body. */
  static SetExpression hole() {
    return new Hole();
  }

  /** Construct a positional selection. */
  static SetExpression select(SetExpression input, long index) {
    return new Select(input, index);
  }

  /** Construct a boolean map, asking which constituents satisfy the body. */
  static SetExpression mapBool(VecSetExpression vector, BoolExpression body) {
    return new MapBool(vector, body);
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
