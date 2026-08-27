package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.math.BigInteger;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;

class SetExpressionTest {
  @Test
  void keyEncodingMatchesTheRustV1Codec() {
    SetExpression expression = SetExpression.key(0x0102_0304_0506_0708L);
    assertArrayEquals(
        HexFormat.of().parseHex("59534e580100010807060504030201"), expression.encode());
    assertEquals(expression, SetExpression.decode(expression.encode()));
  }

  @Test
  void literalEncodingMatchesRustAndNormalizesSetSemantics() {
    SetExpression literal = SetExpression.literal(0, 2, 65_536, -2L);
    assertArrayEquals(
        HexFormat.of()
            .parseHex(
                "59534e5801000904000000000000000000000002000000000000000000010000000000feffffffffffffff"),
        literal.encode());
    assertEquals(literal, SetExpression.decode(literal.encode()));
    assertEquals(SetExpression.literal(1, 5, 9), SetExpression.literal(9, 1, 9, 5));
    assertThrows(IllegalArgumentException.class, () -> SetExpression.literal(-1L));
  }

  @Test
  void nestedAndViewEncodingsRoundTrip() {
    SetExpression expression =
        SetExpression.and(
            SetExpression.key(-1L),
            SetExpression.literal(9, 1, 9),
            new SetExpression.ViewSelect(9, ViewSpec.blocked(3, 100), 2),
            new SetExpression.ViewExpand(SetExpression.range(0, 10), ViewSpec.interleaved(2)));
    assertEquals(expression, SetExpression.decode(expression.encode()));
  }

  @Test
  void derivedBooleanOperationsUseOnlyV1Nodes() {
    SetExpression left = SetExpression.key(1);
    SetExpression right = SetExpression.key(2);
    assertEquals(
        SetExpression.andNot(SetExpression.or(left, right), SetExpression.and(left, right)),
        SetExpression.xor(left, right));
    assertEquals(
        SetExpression.andNot(SetExpression.range(0, -1L), left), SetExpression.complement(left));
  }

  @Test
  void queryRequestMatchesTheRustV1CodecAndRejectsMalformedHeaders() {
    QueryRequest request = QueryRequest.at(SetExpression.key(42), 7);
    byte[] wire =
        HexFormat.of().parseHex("59534e510101070000000000000059534e580100012a00000000000000");
    assertArrayEquals(wire, request.encode());
    assertEquals(request, QueryRequest.decode(wire));
    assertEquals(true, QueryRequest.looksLikeRequest(wire));

    QueryRequest current = QueryRequest.current(SetExpression.range(1, 3));
    assertEquals(current, QueryRequest.decode(current.encode()));

    byte[] unknownFlags = current.encode();
    unknownFlags[5] = 2;
    assertThrows(IllegalArgumentException.class, () -> QueryRequest.decode(unknownFlags));
    byte[] unpinnedVersion = current.encode();
    unpinnedVersion[6] = 1;
    assertThrows(IllegalArgumentException.class, () -> QueryRequest.decode(unpinnedVersion));
    assertThrows(IllegalArgumentException.class, () -> QueryRequest.decode(new byte[0]));
  }

  @Test
  void malformedAndOverlyDeepExpressionsAreRejected() {
    byte[] trailing = SetExpression.key(1).encode();
    trailing = java.util.Arrays.copyOf(trailing, trailing.length + 1);
    byte[] malformed = trailing;
    assertThrows(IllegalArgumentException.class, () -> SetExpression.decode(malformed));
    byte[] nonCanonical =
        HexFormat.of().parseHex("59534e580100090200000002000000000000000100000000000000");
    assertThrows(IllegalArgumentException.class, () -> SetExpression.decode(nonCanonical));

    SetExpression deep = SetExpression.key(1);
    for (int depth = 0; depth <= SetExpression.MAX_DEPTH; depth++) {
      deep = SetExpression.and(deep);
    }
    SetExpression tooDeep = deep;
    assertThrows(IllegalArgumentException.class, tooDeep::encode);
  }

  @Test
  void unsignedConversionsCoverTheWholeDomain() {
    BigInteger maximum = new BigInteger("18446744073709551615");
    assertEquals(-1L, UnsignedLongs.fromBigInteger(maximum));
    assertEquals(maximum, UnsignedLongs.toBigInteger(-1L));
    assertEquals(maximum.toString(), UnsignedLongs.toString(-1L));
    assertEquals(-1L, UnsignedLongs.parse(maximum.toString()));
    assertThrows(
        IllegalArgumentException.class,
        () -> UnsignedLongs.fromBigInteger(BigInteger.ONE.shiftLeft(64)));
  }
}
