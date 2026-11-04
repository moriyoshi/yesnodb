package dev.yesnodb.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.LongBuffer;
import java.util.ArrayList;
import java.util.List;

/**
 * A container payload decoder written from the storage format's table, not from the encoder.
 *
 * <p>It shares nothing with {@link ContainerChunk} on purpose, and reads a bitmap as the table
 * states it -- 1024 little-endian {@code u64} words -- where the encoder writes a flat byte array.
 * So a byte-order or bit-order mistake in either shows as a disagreement rather than cancelling
 * out. Strict where the server is: a payload this refuses, the server refuses too.
 */
final class PayloadDecoder {
  private PayloadDecoder() {}

  /** The 16-bit values a payload holds, ascending. */
  static List<Integer> decode(byte kind, byte[] payload) {
    ByteBuffer bytes = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN);
    List<Integer> values = new ArrayList<>();
    switch (kind) {
      case 0 -> {
        if (payload.length % 2 != 0 || payload.length / 2 > 4096 || payload.length == 0) {
          throw new IllegalArgumentException("array payload of " + payload.length + " bytes");
        }
        int previous = -1;
        while (bytes.hasRemaining()) {
          int value = Short.toUnsignedInt(bytes.getShort());
          if (value <= previous) {
            throw new IllegalArgumentException("array values do not ascend");
          }
          values.add(value);
          previous = value;
        }
      }
      case 1 -> {
        if (payload.length != 8192) {
          throw new IllegalArgumentException("bitmap payload of " + payload.length + " bytes");
        }
        LongBuffer words = bytes.asLongBuffer();
        for (int word = 0; word < 1024; word++) {
          long bits = words.get(word);
          for (int bit = 0; bit < 64; bit++) {
            if ((bits >>> bit & 1) != 0) {
              values.add(word * 64 + bit);
            }
          }
        }
      }
      case 2 -> {
        int runs = Short.toUnsignedInt(bytes.getShort());
        if (payload.length != 2 + 4 * runs || runs == 0) {
          throw new IllegalArgumentException(
              "run payload of " + payload.length + " bytes declares " + runs + " runs");
        }
        int next = 0;
        for (int run = 0; run < runs; run++) {
          int start = Short.toUnsignedInt(bytes.getShort());
          int last = start + Short.toUnsignedInt(bytes.getShort());
          // Strictly above the previous end + 1: adjacent runs are legal in a
          // foreign file, but an encoder that emits them is not maximal.
          if (start < next || (run > 0 && start == next) || last > 65535) {
            throw new IllegalArgumentException("run " + run + " overlaps, touches or overflows");
          }
          for (int value = start; value <= last; value++) {
            values.add(value);
          }
          next = last + 1;
        }
      }
      default -> throw new IllegalArgumentException("unknown kind " + kind);
    }
    return values;
  }
}
