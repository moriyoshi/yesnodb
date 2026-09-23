package dev.yesnodb.client;

/**
 * One row of the mixed-operation DoPut schema {@code ( key, lo, hi, op )}.
 *
 * <p>Build these with the factory methods rather than the constructor: a single-ordinal operation
 * is a range whose bounds are equal, and the wire cannot detect a caller getting that wrong.
 *
 * <p>Every component carries raw unsigned 64-bit bits, as elsewhere in this client.
 */
public record Mutation(long key, long lo, long hi, byte op) {

  /** Row operations. These numbers go on the wire and are never reordered. */
  public static final byte OP_INSERT = 0;

  public static final byte OP_REMOVE = 1;
  public static final byte OP_INSERT_RANGE = 2;
  public static final byte OP_REMOVE_RANGE = 3;
  public static final byte OP_DELETE_KEY = 4;

  /** Add one ordinal to a key. */
  public static Mutation insert(long key, long ordinal) {
    return new Mutation(key, ordinal, ordinal, OP_INSERT);
  }

  /** Drop one ordinal from a key. */
  public static Mutation remove(long key, long ordinal) {
    return new Mutation(key, ordinal, ordinal, OP_REMOVE);
  }

  /** Add the inclusive range {@code [lo, hi]} to a key. */
  public static Mutation insertRange(long key, long lo, long hi) {
    return new Mutation(key, lo, hi, OP_INSERT_RANGE);
  }

  /** Drop the inclusive range {@code [lo, hi]} from a key. */
  public static Mutation removeRange(long key, long lo, long hi) {
    return new Mutation(key, lo, hi, OP_REMOVE_RANGE);
  }

  /** Drop every ordinal under a key. */
  public static Mutation deleteKey(long key) {
    return new Mutation(key, 0, 0, OP_DELETE_KEY);
  }

  /**
   * Rejects everything the server rejects, so a malformed mutation fails here rather than after a
   * round trip and a partially staged transaction.
   */
  public Mutation {
    switch (op) {
      case OP_DELETE_KEY -> {
        // Carries no ordinals, so the ceiling and the ordering do not apply --
        // but the server refuses a whole-key delete that carries bounds rather
        // than ignoring them, so this does too.
        if (lo != 0 || hi != 0) {
          throw new IllegalArgumentException(
              "delete of key "
                  + UnsignedLongs.toString(key)
                  + " carries bounds "
                  + UnsignedLongs.toString(lo)
                  + "..="
                  + UnsignedLongs.toString(hi)
                  + "; it names no range");
        }
      }
      case OP_INSERT, OP_REMOVE, OP_INSERT_RANGE, OP_REMOVE_RANGE -> {
        if (lo == -1L || hi == -1L) {
          throw new IllegalArgumentException(
              "the maximum unsigned 64-bit value is reserved and cannot be stored as an ordinal");
        }
        if (Long.compareUnsigned(lo, hi) > 0) {
          throw new IllegalArgumentException(
              "range lower bound "
                  + UnsignedLongs.toString(lo)
                  + " is above upper bound "
                  + UnsignedLongs.toString(hi));
        }
        // A point operation is a range whose bounds are equal. Sending one
        // with hi != lo is a transposed argument, not a range.
        if ((op == OP_INSERT || op == OP_REMOVE) && lo != hi) {
          throw new IllegalArgumentException(
              "point operation on key "
                  + UnsignedLongs.toString(key)
                  + " has lo "
                  + UnsignedLongs.toString(lo)
                  + " and hi "
                  + UnsignedLongs.toString(hi)
                  + "; set hi == lo, or use the range operation");
        }
      }
      default -> throw new IllegalArgumentException("unknown mutation op " + op);
    }
  }
}
