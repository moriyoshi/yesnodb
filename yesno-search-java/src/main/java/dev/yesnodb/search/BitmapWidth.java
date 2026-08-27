package dev.yesnodb.search;

/** The numeric field and portable Roaring representation targeted by a search query. */
public enum BitmapWidth {
  /** A 32-bit Roaring bitmap for a non-negative {@code integer} field. */
  INTEGER_32,

  /** A portable 64-bit Roaring bitmap for a non-negative {@code long} field. */
  LONG_64
}
