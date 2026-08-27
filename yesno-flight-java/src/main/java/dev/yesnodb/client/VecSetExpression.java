package dev.yesnodb.client;

import java.util.Arrays;
import java.util.List;
import java.util.Objects;

/**
 * A fixed-arity vector of sets -- what a view produces.
 *
 * <p>A separate sealed interface from {@link SetExpression} so that an ill-sorted tree does not
 * compile, leaving the codec to check only what arrives as bytes. Vectors do not nest: the element
 * sort is a set, never another vector.
 */
public sealed interface VecSetExpression
    permits VecSetExpression.Listing, VecSetExpression.View, VecSetExpression.MapSet {

  /**
   * How many elements this vector has.
   *
   * <p>Always statically known, which is what makes {@link SetExpression.At} total.
   */
  int arity();

  /** A literal vector, {@code [ a, b, c ]}. */
  record Listing(List<SetExpression> elements) implements VecSetExpression {
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

  /** Read one set as {@code view.sets()} constituents -- the curry direction. */
  record View(SetExpression input, ViewSpec view) implements VecSetExpression {
    /** Construct a view of a set expression. */
    public View {
      Objects.requireNonNull(input, "input");
      Objects.requireNonNull(view, "view");
    }

    @Override
    public int arity() {
      return (int) view.sets();
    }
  }

  /**
   * A set-valued query applied to every element -- {@code map(v, and(_, q))}.
   *
   * <p>{@code map} is functorial for any body, so unlike {@link FoldOp} there is no closure
   * theorem constraining what may appear here.
   */
  record MapSet(VecSetExpression vector, SetExpression body) implements VecSetExpression {
    /** Construct a set-valued map. */
    public MapSet {
      Objects.requireNonNull(vector, "vector");
      Objects.requireNonNull(body, "body");
    }

    @Override
    public int arity() {
      return vector.arity();
    }
  }

  /** Construct a set-valued map. */
  static VecSetExpression map(VecSetExpression vector, SetExpression body) {
    return new MapSet(vector, body);
  }

  /** Construct a literal vector. */
  static VecSetExpression list(SetExpression... elements) {
    return new Listing(Arrays.asList(elements));
  }

  /** Construct a view of a set expression. */
  static VecSetExpression view(SetExpression input, ViewSpec view) {
    return new View(input, view);
  }
}
