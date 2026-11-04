package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Random;
import java.util.TreeSet;
import java.util.stream.IntStream;
import org.junit.jupiter.api.Test;
import org.roaringbitmap.RoaringBitmap;

class ContainerChunkTest {
  private static byte[] bytes(int... values) {
    byte[] out = new byte[values.length];
    for (int index = 0; index < values.length; index++) {
      out[index] = (byte) values[index];
    }
    return out;
  }

  // ---- the layouts, as literal bytes, so a byte-order mistake cannot hide
  // behind a decoder that shares it.

  @Test
  void anArrayIsLittleEndianValues() {
    ContainerChunk chunk = ContainerChunk.encode(1, 0, new int[] {1, 0x0203});
    assertEquals(ContainerChunk.KIND_ARRAY, chunk.kind());
    assertArrayEquals(bytes(0x01, 0x00, 0x03, 0x02), chunk.payload());
  }

  @Test
  void aRunIsACountThenStartAndLengthMinusOne() {
    ContainerChunk chunk =
        ContainerChunk.encode(1, 0, IntStream.concat(IntStream.range(10, 20), IntStream.of(300)).toArray());
    assertEquals(ContainerChunk.KIND_RUN, chunk.kind());
    assertEquals(11, chunk.cardinality());
    assertArrayEquals(
        bytes(0x02, 0x00, 0x0a, 0x00, 0x09, 0x00, 0x2c, 0x01, 0x00, 0x00), chunk.payload());
  }

  @Test
  void aBitmapIsLsbFirstWithinLittleEndianWords() {
    // Every even value: 32 768 of them, too many for an array, and 32 768
    // single-value runs, so only a bitmap fits.
    int[] evens = IntStream.range(0, 32768).map(value -> 2 * value).toArray();
    ContainerChunk chunk = ContainerChunk.encode(1, 0, evens);
    assertEquals(ContainerChunk.KIND_BITMAP, chunk.kind());
    byte[] payload = chunk.payload();
    assertEquals(ContainerChunk.BITMAP_BYTES, payload.length);
    for (byte b : payload) {
      assertEquals(0x55, b & 0xff);
    }
    // And the highest value lands in the top bit of the last byte.
    int[] withTop =
        IntStream.concat(IntStream.range(0, 5000).map(value -> 2 * value), IntStream.of(65535))
            .toArray();
    ContainerChunk top = ContainerChunk.encode(1, 0, withTop);
    assertEquals(ContainerChunk.KIND_BITMAP, top.kind());
    assertEquals(0x80, top.payload()[8191] & 0xff);
    assertEquals(0x55, top.payload()[0] & 0xff);
  }

  @Test
  void aTieGoesToTheLowestKind() {
    // Three consecutive values: an array of 6 bytes and one run of 6 bytes.
    assertEquals(ContainerChunk.KIND_ARRAY, ContainerChunk.encode(1, 0, new int[] {4, 5, 6}).kind());
    // 4096 scattered values: an array of 8192 bytes ties the bitmap.
    int[] spread = IntStream.range(0, 4096).map(value -> 16 * value).toArray();
    assertEquals(ContainerChunk.KIND_ARRAY, ContainerChunk.encode(1, 0, spread).kind());
    // One more value and the array is no longer allowed at all.
    int[] over = IntStream.range(0, 4097).map(value -> 15 * value).toArray();
    assertEquals(ContainerChunk.KIND_BITMAP, ContainerChunk.encode(1, 0, over).kind());
  }

  // ---- the randomized sweep: every encoding decodes to its input, is the
  // smallest viable one, and the sweep actually produced all three.

  @Test
  void everyChunkRoundTripsAndNothingSmallerWouldDo() {
    Random random = new Random(20261027L);
    int[] seen = new int[3];
    int multiIntervalRuns = 0;
    for (int trial = 0; trial < 3000; trial++) {
      int[] values = randomChunk(random);
      ContainerChunk chunk = ContainerChunk.encode(7, trial, values);
      List<Integer> decoded = PayloadDecoder.decode(chunk.kind(), chunk.payload());
      assertEquals(Arrays.stream(values).boxed().toList(), decoded, "trial " + trial);
      assertEquals(values.length, chunk.cardinality());

      int runs = runs(values);
      int smallest = Math.min(8192, 2 + 4 * runs);
      if (values.length <= 4096) {
        smallest = Math.min(smallest, 2 * values.length);
      }
      assertEquals(smallest, chunk.payloadLength(), "trial " + trial + " chose " + chunk.kind());
      seen[chunk.kind()]++;
      if (chunk.kind() == ContainerChunk.KIND_RUN && runs > 1) {
        multiIntervalRuns++;
      }
    }
    // The coverage promise is asserted, not hoped for: a sweep that never put
    // a multi-interval run through the encoder would pass with its run loop
    // broken, which is exactly what the Python and Go sweeps once did.
    for (int kind = 0; kind < 3; kind++) {
      assertTrue(seen[kind] > 100, "kind " + kind + " appeared " + seen[kind] + " times");
    }
    assertTrue(multiIntervalRuns > 100, multiIntervalRuns + " multi-interval runs");
  }

  private static int[] randomChunk(Random random) {
    TreeSet<Integer> values = new TreeSet<>();
    switch (random.nextInt(4)) {
      case 0 -> {
        // sparse
        int count = 1 + random.nextInt(random.nextBoolean() ? 16 : 6000);
        while (values.size() < count) {
          values.add(random.nextInt(65536));
        }
      }
      case 1 -> {
        // a few intervals, of any length
        int intervals = 1 + random.nextInt(random.nextBoolean() ? 4 : 600);
        for (int interval = 0; interval < intervals; interval++) {
          int start = random.nextInt(65536);
          int length = 1 + random.nextInt(random.nextBoolean() ? 8 : 4000);
          for (int value = start; value < Math.min(65536, start + length); value++) {
            values.add(value);
          }
        }
      }
      case 2 -> {
        // dense, with holes
        double keep = 0.3 + 0.7 * random.nextDouble();
        for (int value = 0; value < 65536; value++) {
          if (random.nextDouble() < keep) {
            values.add(value);
          }
        }
        if (values.isEmpty()) {
          values.add(0);
        }
      }
      default -> {
        // the edges of the value range
        values.add(0);
        values.add(65535);
        if (random.nextBoolean()) {
          for (int value = 65535 - random.nextInt(100); value < 65536; value++) {
            values.add(value);
          }
        }
      }
    }
    return values.stream().mapToInt(Integer::intValue).toArray();
  }

  private static int runs(int[] values) {
    int runs = 1;
    for (int index = 1; index < values.length; index++) {
      if (values[index] != values[index - 1] + 1) {
        runs++;
      }
    }
    return runs;
  }

  // ---- from whole sets

  @Test
  void ordinalsAreChunkedInUnsignedOrderWithRepeatsDropped() {
    long top = (1L << 48) - 1;
    long[] ordinals = {
      -2L, // the largest storable ordinal: prefix 2^48 - 1, value 65534
      5, 3, 5, 70000, 3, (1L << 40) + 9, -3L,
    };
    List<ContainerChunk> chunks = ContainerChunk.ofOrdinals(9, ordinals);
    List<Long> prefixes = new ArrayList<>();
    TreeSet<Long> back = new TreeSet<>(Long::compareUnsigned);
    for (ContainerChunk chunk : chunks) {
      assertEquals(9, chunk.key());
      prefixes.add(chunk.prefix48());
      for (int value : PayloadDecoder.decode(chunk.kind(), chunk.payload())) {
        back.add(chunk.prefix48() << 16 | value);
      }
    }
    assertEquals(List.of(0L, 1L, 1L << 24, top), prefixes);
    TreeSet<Long> want = new TreeSet<>(Long::compareUnsigned);
    for (long ordinal : ordinals) {
      want.add(ordinal);
    }
    assertEquals(want, back);
    assertEquals(want.size(), chunks.stream().mapToInt(ContainerChunk::cardinality).sum());
    assertEquals(List.of(), ContainerChunk.ofOrdinals(9));
  }

  @Test
  void aBitmapIsReadAsUnsigned() {
    RoaringBitmap bitmap = RoaringBitmap.bitmapOf(0, 1, 2, 65536, -1, Integer.MIN_VALUE);
    List<ContainerChunk> chunks = ContainerChunk.ofBitmap(3, bitmap);
    assertEquals(List.of(0L, 1L, 0x8000L, 0xffffL), chunks.stream().map(ContainerChunk::prefix48).toList());
    assertEquals(List.of(0, 1, 2), PayloadDecoder.decode(chunks.get(0).kind(), chunks.get(0).payload()));
    assertEquals(List.of(65535), PayloadDecoder.decode(chunks.get(3).kind(), chunks.get(3).payload()));
    assertEquals(chunks, ContainerChunk.ofOrdinals(3, 0, 1, 2, 65536, 0xffff_ffffL, 0x8000_0000L));
  }

  // ---- refusals

  @Test
  void whatTheServerWouldRefuseIsRefusedHere() {
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 0, new int[0]));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 0, new int[] {2, 1}));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 0, new int[] {2, 2}));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 0, new int[] {-1}));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 0, new int[] {65536}));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.encode(1, 1L << 48, new int[] {1}));
    assertThrows(IllegalArgumentException.class, () -> ContainerChunk.ofOrdinals(1, 4, -1L));
    assertThrows(
        IllegalArgumentException.class, () -> new ContainerChunk(1, 0, (byte) 3, 1, new byte[2]));
    assertThrows(
        IllegalArgumentException.class, () -> new ContainerChunk(1, 0, (byte) 0, 0, new byte[0]));
    assertThrows(
        IllegalArgumentException.class, () -> new ContainerChunk(1, 0, (byte) 1, 65537, new byte[8192]));
  }

  @Test
  void aChunkCannotBeChangedThroughItsPayload() {
    byte[] payload = bytes(1, 0);
    ContainerChunk chunk = new ContainerChunk(1, 0, ContainerChunk.KIND_ARRAY, 1, payload);
    payload[0] = 9;
    chunk.payload()[0] = 9;
    assertArrayEquals(bytes(1, 0), chunk.payload());
    assertEquals(chunk, new ContainerChunk(1, 0, ContainerChunk.KIND_ARRAY, 1, bytes(1, 0)));
  }
}
