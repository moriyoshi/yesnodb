package dev.yesnodb.client;

/**
 * How {@link SetExpression.Fold} combines a vector's elements.
 *
 * <p>Exactly three, and provably so: on {@code {0,1}} an associative, unital operation is
 * automatically commutative and is one of four -- {@code or}, {@code xor} with unit 0 and {@code
 * and}, {@code iff} with unit 1. {@code iff} keeps a fibre of zeros, so its result is dense however
 * sparse the operand, and {@code andnot} is neither associative nor commutative.
 */
public enum FoldOp {
  /** Union -- set when any element holds the ordinal. */
  OR,
  /** Intersection -- set when every element holds it. */
  AND,
  /** Symmetric difference -- set when an odd number hold it. */
  XOR
}
