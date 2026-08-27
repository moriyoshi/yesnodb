package dev.yesnodb.client;

/** Reduction across every constituent of a packed view. */
public enum ViewReduce {
  /** Union. */
  ANY,
  /** Intersection. */
  ALL,
  /** Symmetric difference. */
  PARITY
}
