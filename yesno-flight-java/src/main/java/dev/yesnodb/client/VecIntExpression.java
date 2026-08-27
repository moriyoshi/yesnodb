package dev.yesnodb.client;

import java.util.Arrays;
import java.util.List;
import java.util.Objects;

/**
 * One integer per constituent.
 *
 * <p>Always exactly the vector's arity long. A reduction indexed by <em>ordinal</em> must stay
 * sparse -- the logical universe is far too large to enumerate -- so only one indexed by
 * <em>constituent</em> may be dense, which is what makes this sort unambiguous about which axis a
 * reduction collapsed.
 */
public sealed interface VecIntExpression
    permits VecIntExpression.Listing, VecIntExpression.MapInt {

  /** How many elements this vector has. */
  int arity();

  /** A literal vector of integers. */
  record Listing(List<IntExpression> elements) implements VecIntExpression {
    /** Construct a literal vector and take an immutable copy of its elements. */
    public Listing {
      Objects.requireNonNull(elements, "elements");
      if (elements.isEmpty()) {
        throw new IllegalArgumentException("a vector must have at least one element");
      }
      elements = List.copyOf(elements);
    }

    @Override
    public int arity() {
      return elements.size();
    }
  }

  /**
   * A scalar query applied to every element of a vector of sets -- the facet histogram.
   *
   * <p>This is a <em>map</em>, not a fold: it does not combine the elements, it applies a query to
   * each. The two are the marginals of the same matrix and do not determine each other.
   */
  record MapInt(VecSetExpression vector, IntExpression body) implements VecIntExpression {
    /** Construct an integer-valued map. */
    public MapInt {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(body, "body");
    }

    @Override
    public int arity() {
      return vector.arity();
    }
  }

  /** Construct a literal vector of integers. */
  static VecIntExpression list(IntExpression... elements) {
    return new Listing(Arrays.asList(elements));
  }

  /** Construct an integer-valued map. */
  static VecIntExpression map(VecSetExpression vector, IntExpression body) {
    return new MapInt(vector, body);
  }

  /** Encode this vector, including the {@code YSNX} v1 header. */
  default byte[] encode() {
    return SetExpressionCodec.encodeIntVector(this);
  }

  /** Decode and validate one complete v1 integer-vector query. */
  static VecIntExpression decode(byte[] encoded) {
    Objects.requireNonNull(encoded, "encoded");
    return SetExpressionCodec.decodeIntVector(encoded);
  }
}
