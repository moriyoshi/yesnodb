package dev.yesnodb.search;

import java.util.Arrays;
import java.util.Base64;
import java.util.Objects;

/** An immutable, snapshot-labelled portable Roaring bitmap ready for a search query. */
public record PreparedBitmap(
    BitmapWidth width, long yesnoVersion, long cardinality, byte[] portableBytes) {
  /** Validate and defensively retain the prepared value. */
  public PreparedBitmap {
    width = Objects.requireNonNull(width, "width");
    if (cardinality < 0) {
      throw new IllegalArgumentException("cardinality must not be negative");
    }
    portableBytes = Objects.requireNonNull(portableBytes, "portableBytes").clone();
  }

  /** Return a defensive copy of the portable Roaring bytes. */
  @Override
  public byte[] portableBytes() {
    return portableBytes.clone();
  }

  /** Return the portable bytes encoded for a JSON bitmap query. */
  public String base64() {
    return Base64.getEncoder().encodeToString(portableBytes);
  }

  /** Compare bitmap content rather than array identity. */
  @Override
  public boolean equals(Object other) {
    return other instanceof PreparedBitmap bitmap
        && width == bitmap.width
        && yesnoVersion == bitmap.yesnoVersion
        && cardinality == bitmap.cardinality
        && Arrays.equals(portableBytes, bitmap.portableBytes);
  }

  /** Hash bitmap content rather than array identity. */
  @Override
  public int hashCode() {
    int result = Objects.hash(width, yesnoVersion, cardinality);
    return 31 * result + Arrays.hashCode(portableBytes);
  }
}
