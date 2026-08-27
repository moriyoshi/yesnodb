package dev.yesnodb.client;

/** Describes how a packed view's constituent sets share one physical ordinal space. */
public record ViewSpec(long sets, Layout layout, long stride) {
  /** The supported physical layouts. */
  public enum Layout {
    /** Physical ordinal {@code logical * sets + constituent}. */
    INTERLEAVED,
    /** Physical ordinal {@code constituent * stride + logical}. */
    BLOCKED
  }

  /** Validate and construct a packed-view descriptor. */
  public ViewSpec {
    if (sets <= 0 || sets > 0xffff_ffffL) {
      throw new IllegalArgumentException("sets must be in 1..2^32-1");
    }
    if (layout == null) {
      throw new NullPointerException("layout");
    }
    if (layout == Layout.INTERLEAVED && stride != 0) {
      throw new IllegalArgumentException("an interleaved view has no stride");
    }
    if (layout == Layout.BLOCKED && stride == 0) {
      throw new IllegalArgumentException("a blocked view must have a non-zero stride");
    }
  }

  /** Create an interleaved descriptor. */
  public static ViewSpec interleaved(long sets) {
    return new ViewSpec(sets, Layout.INTERLEAVED, 0);
  }

  /** Create a blocked descriptor; {@code stride} carries raw unsigned 64-bit bits. */
  public static ViewSpec blocked(long sets, long stride) {
    return new ViewSpec(sets, Layout.BLOCKED, stride);
  }
}
