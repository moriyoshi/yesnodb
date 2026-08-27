package dev.yesnodb.search;

/** Selects the yesnodb snapshot used to resolve a search bitmap. */
public sealed interface SnapshotMode permits SnapshotMode.Current, SnapshotMode.Pinned {
  /** Resolve at the current snapshot and completely restart a retryable failed attempt. */
  record Current(int maxRetries) implements SnapshotMode {
    /** Validate the retry count. */
    public Current {
      if (maxRetries < 0) {
        throw new IllegalArgumentException("maxRetries must not be negative");
      }
    }
  }

  /** Resolve at exactly one retained yesnodb version and never fall forward. */
  record Pinned(long version) implements SnapshotMode {}
}
