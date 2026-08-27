package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

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
            SetExpression.at(
                VecSetExpression.view(SetExpression.key(9), ViewSpec.blocked(3, 100)), 2),
            // The composition the old leaf nodes could not express: the folded
            // operand is computed rather than a bare key.
            SetExpression.fold(
                VecSetExpression.view(
                    SetExpression.andNot(SetExpression.key(60), SetExpression.key(61)),
                    ViewSpec.interleaved(3)),
                FoldOp.OR),
            SetExpression.pack(
                VecSetExpression.list(SetExpression.key(80), SetExpression.key(81)),
                ViewSpec.interleaved(2)),
            SetExpression.expand(SetExpression.range(0, 10), ViewSpec.interleaved(2)));
    assertEquals(expression, SetExpression.decode(expression.encode()));
  }

  /**
   * Mirrors {@code the_cross_implementation_wire_vector_is_stable} in the Rust crate byte for byte.
   * Five implementations of one format drift silently otherwise -- each round-trips against itself
   * while disagreeing with the others, and only a shared constant catches it.
   */
  @Test
  void theCrossImplementationWireVectorIsStable() {
    SetExpression expression =
        SetExpression.at(
            VecSetExpression.view(SetExpression.key(9), ViewSpec.interleaved(3)), 1);
    assertArrayEquals(
        HexFormat.of()
            .parseHex("59534e580100060c0300000000000000000000000001090000000000000001000000"),
        expression.encode());
    assertEquals(expression, SetExpression.decode(expression.encode()));
  }

  /** Arity and index are statically known, so both are refused at construction. */
  @Test
  void arityAndIndexAreCheckedWhenBuilding() {
    VecSetExpression two =
        VecSetExpression.list(SetExpression.key(1), SetExpression.key(2));
    assertThrows(IllegalArgumentException.class, () -> SetExpression.at(two, 2));
    assertThrows(
        IllegalArgumentException.class,
        () -> SetExpression.pack(two, ViewSpec.interleaved(3)));
    assertEquals(2, two.arity());
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

  /**
   * The facet query: per-constituent counts under a filter. Mirrors the tests of the same name in
   * the Rust, Python and Go implementations.
   *
   * <p>It is a <em>map</em>, not a fold -- it applies a query to each constituent rather than
   * combining them -- and its result is the row marginal, which no fold can produce.
   */
  @Test
  void theFacetQueryRoundTrips() {
    VecIntExpression facet =
        VecIntExpression.map(
            VecSetExpression.view(SetExpression.key(9), ViewSpec.interleaved(4)),
            IntExpression.cardinality(
                SetExpression.and(SetExpression.hole(), SetExpression.key(7))));
    assertEquals(facet, VecIntExpression.decode(facet.encode()));
    assertEquals(4, facet.arity(), "a map preserves shape");

    IllegalArgumentException wrongSort =
        assertThrows(IllegalArgumentException.class, () -> SetExpression.decode(facet.encode()));
    assertTrue(wrongSort.getMessage().contains("where a set was required"));
  }

  /**
   * The hole is legal only inside a map body, and a body may not contain a map -- but a map in a
   * <em>vector</em> position is sequential rather than nested and must still decode.
   */
  @Test
  void theHoleIsScopedToAMapBody() {
    IllegalArgumentException bare =
        assertThrows(
            IllegalArgumentException.class,
            () -> SetExpression.decode(SetExpression.hole().encode()));
    assertTrue(bare.getMessage().contains("outside a map body"));

    VecSetExpression two = VecSetExpression.list(SetExpression.key(1), SetExpression.key(2));
    SetExpression nested =
        SetExpression.fold(
            VecSetExpression.map(
                two,
                SetExpression.fold(
                    VecSetExpression.map(two, SetExpression.hole()), FoldOp.OR)),
            FoldOp.OR);
    IllegalArgumentException inner =
        assertThrows(
            IllegalArgumentException.class, () -> SetExpression.decode(nested.encode()));
    assertTrue(inner.getMessage().contains("may not contain a map"));

    SetExpression sequential =
        SetExpression.fold(
            VecSetExpression.map(
                VecSetExpression.map(two, SetExpression.hole()), SetExpression.hole()),
            FoldOp.OR);
    assertEquals(sequential, SetExpression.decode(sequential.encode()));
  }
}
