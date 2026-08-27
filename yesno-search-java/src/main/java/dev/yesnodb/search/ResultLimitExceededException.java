package dev.yesnodb.search;

/** A planned yesnodb result exceeds the configured materialization ceiling. */
public final class ResultLimitExceededException extends IllegalArgumentException {
  private static final long serialVersionUID = 1L;

  private final long promised;
  private final long limit;

  ResultLimitExceededException(long promised, long limit) {
    super("yesnodb promised " + promised + " matches, exceeding the configured limit " + limit);
    this.promised = promised;
    this.limit = limit;
  }

  /** Exact cardinality promised by yesnodb. */
  public long promised() {
    return promised;
  }

  /** Configured maximum cardinality. */
  public long limit() {
    return limit;
  }
}
