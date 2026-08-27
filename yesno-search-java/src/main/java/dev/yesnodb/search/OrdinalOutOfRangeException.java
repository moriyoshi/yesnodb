package dev.yesnodb.search;

/** A yesnodb unsigned ordinal cannot be represented by the selected search field domain. */
public final class OrdinalOutOfRangeException extends ArithmeticException {
  private static final long serialVersionUID = 1L;

  private final long ordinal;
  private final BitmapWidth width;

  OrdinalOutOfRangeException(long ordinal, BitmapWidth width) {
    super(
        "yesnodb ordinal "
            + Long.toUnsignedString(ordinal)
            + " is outside the non-negative "
            + width
            + " search domain");
    this.ordinal = ordinal;
    this.width = width;
  }

  /** Raw unsigned-64 bits of the rejected ordinal. */
  public long ordinal() {
    return ordinal;
  }

  /** Requested search bitmap width. */
  public BitmapWidth width() {
    return width;
  }
}
