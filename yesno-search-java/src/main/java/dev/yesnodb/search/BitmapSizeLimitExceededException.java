package dev.yesnodb.search;

/** A serialized search bitmap exceeds the configured byte ceiling. */
public final class BitmapSizeLimitExceededException extends IllegalArgumentException {
  private static final long serialVersionUID = 1L;

  private final long actual;
  private final long limit;

  BitmapSizeLimitExceededException(long actual, long limit) {
    super("serialized bitmap uses " + actual + " bytes, exceeding the configured limit " + limit);
    this.actual = actual;
    this.limit = limit;
  }

  /** Serialized byte count. */
  public long actual() {
    return actual;
  }

  /** Configured byte ceiling. */
  public long limit() {
    return limit;
  }
}
