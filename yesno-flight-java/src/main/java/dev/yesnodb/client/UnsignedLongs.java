package dev.yesnodb.client;

import java.math.BigInteger;
import java.util.Objects;

/** Conversions between Java values and the unsigned 64-bit values used by yesnodb. */
public final class UnsignedLongs {
  /** The largest value representable by an unsigned 64-bit integer. */
  public static final BigInteger MAX_VALUE =
      BigInteger.ONE.shiftLeft(Long.SIZE).subtract(BigInteger.ONE);

  private UnsignedLongs() {}

  /** Parse an unsigned decimal integer and return its raw {@code long} bits. */
  public static long parse(String value) {
    return Long.parseUnsignedLong(value);
  }

  /** Render raw {@code long} bits as an unsigned decimal integer. */
  public static String toString(long value) {
    return Long.toUnsignedString(value);
  }

  /** Convert raw {@code long} bits to a non-negative {@link BigInteger}. */
  public static BigInteger toBigInteger(long value) {
    if (value >= 0) {
      return BigInteger.valueOf(value);
    }
    return BigInteger.valueOf(value & Long.MAX_VALUE).setBit(Long.SIZE - 1);
  }

  /** Convert an unsigned 64-bit {@link BigInteger} to its raw {@code long} bits. */
  public static long fromBigInteger(BigInteger value) {
    Objects.requireNonNull(value, "value");
    if (value.signum() < 0 || value.compareTo(MAX_VALUE) > 0) {
      throw new IllegalArgumentException("value is outside the unsigned 64-bit range: " + value);
    }
    return value.longValue();
  }
}
