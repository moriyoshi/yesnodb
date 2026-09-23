package dev.yesnodb.client;

/**
 * Server space and reader counters; every component carries raw unsigned 64-bit bits.
 *
 * <p>{@code features} is the server's capability bitmask. A server predating the field reports
 * zero, because protobuf decodes an absent field as its default.
 */
public record ServerStats(
    long allocatedBytes,
    long deferredBytes,
    long walBytes,
    long liveReaders,
    long shards,
    long features) {

  /** Mixed-operation {@code apply} DoPut streams. */
  public static final long FEATURE_MIXED_PUT = 1L << 0;

  /** Write transactions spanning several calls. */
  public static final long FEATURE_WRITE_TRANSACTIONS = 1L << 1;

  /** Whether every bit in {@code wanted} is implemented. */
  public boolean supports(long wanted) {
    return (features & wanted) == wanted;
  }
}
