package dev.yesnodb.search;

/** Resource ceilings applied before a prepared bitmap is returned. */
public record ResolveLimits(long maxMatches, long maxSerializedBytes) {
  /** Validate both inclusive ceilings. */
  public ResolveLimits {
    if (maxMatches < 0) {
      throw new IllegalArgumentException("maxMatches must not be negative");
    }
    if (maxSerializedBytes < 0) {
      throw new IllegalArgumentException("maxSerializedBytes must not be negative");
    }
  }

  /** A conservative default suitable for interactive search requests. */
  public static ResolveLimits defaults() {
    return new ResolveLimits(10_000_000, 64L * 1024 * 1024);
  }
}
