package dev.yesnodb.client;

import java.util.Objects;

/** A single truth value. Only ever a map body -- never a query result. */
public sealed interface BoolExpression permits BoolExpression.Contains {

  /** Whether a set holds one ordinal. */
  record Contains(SetExpression input, long ordinal) implements BoolExpression {
    /** Construct a membership test. */
    public Contains {
      Objects.requireNonNull(input, "input");
    }
  }

  /** Construct a membership test. */
  static BoolExpression contains(SetExpression input, long ordinal) {
    return new Contains(input, ordinal);
  }
}
