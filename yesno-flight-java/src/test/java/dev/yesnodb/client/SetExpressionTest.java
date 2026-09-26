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

  /**
   * Mirrors {@code the_big_cross_implementation_wire_vector_is_stable} in the Rust crate, and its
   * Python and Go counterparts.
   *
   * <p>Five implementations of one format drift silently otherwise: each can round-trip against
   * itself while disagreeing with the others, and only a shared constant catches that.
   */
  @Test
  void bigWireVectorMatchesTheRustCrate() {
    BigExpression e =
        new BigExpression.Saturate(
            new BigExpression.Multiply(
                new BigExpression.ReadUint(SetExpression.key(4), 128),
                new BigExpression.Literal(BigInteger.valueOf(-3))),
            32);
    byte[] encoded = SetExpressionCodec.encodeBig(e);
    assertEquals(
        "59534e58010023200000001f1a8000000001040000000000000018010100000003",
        HexFormat.of().formatHex(encoded));
    assertEquals(e, SetExpressionCodec.decodeBig(encoded));
  }

  @Test
  void bigNodesRoundTrip() {
    BigExpression[] cases = {
      new BigExpression.Literal(BigInteger.ZERO),
      new BigExpression.Literal(BigInteger.ONE),
      new BigExpression.Literal(BigInteger.valueOf(-1)),
      new BigExpression.Literal(BigInteger.ONE.shiftLeft(200)),
      new BigExpression.Literal(BigInteger.ONE.shiftLeft(200).negate()),
      new BigExpression.Widen(IntExpression.cardinality(SetExpression.key(7))),
      new BigExpression.ReadUint(SetExpression.key(4), 128),
      new BigExpression.ReadInt(SetExpression.key(4), 8),
      new BigExpression.Negate(new BigExpression.Literal(BigInteger.valueOf(5))),
      new BigExpression.Add(
          new BigExpression.Literal(BigInteger.ONE),
          new BigExpression.Literal(BigInteger.TWO)),
      new BigExpression.Truncate(new BigExpression.Literal(BigInteger.valueOf(300)), 8),
      new BigExpression.Saturate(new BigExpression.Literal(BigInteger.valueOf(300)), 8),
    };
    for (BigExpression e : cases) {
      assertEquals(e, SetExpressionCodec.decodeBig(SetExpressionCodec.encodeBig(e)));
    }
  }

  /**
   * A zip and a scale encode exactly the bytes the Rust encoder produces.
   *
   * <p>The reference strings come from yesno-wire, which is the authority on the format. A client
   * checked only against its own decoder can be self-consistently wrong.
   */
  @Test
  void bigZipAndScaleMatchTheRustEncoder() {
    String[] want = {
      "59534e580100280024020018000100000007180001000000082402001800010000000118000100000002",
      "59534e5801002900250c030000000000000000000000000109000000000000001a800000000d18000100000003",
      "59534e580100280124020018000100000007180001000000082402001800010000000118000100000002",
      "59534e5801002901250c030000000000000000000000000109000000000000001a800000000d18000100000003",
      "59534e580100280224020018000100000007180001000000082402001800010000000118000100000002",
      "59534e5801002902250c030000000000000000000000000109000000000000001a800000000d18000100000003",
      "59534e580100280324020018000100000007180001000000082402001800010000000118000100000002",
      "59534e5801002903250c030000000000000000000000000109000000000000001a800000000d18000100000003",
      "59534e580100280424020018000100000007180001000000082402001800010000000118000100000002",
      "59534e5801002904250c030000000000000000000000000109000000000000001a800000000d18000100000003",
    };
    HexFormat hex = HexFormat.of();
    for (BigBinOp op : BigBinOp.values()) {
      VecBigExpression zip =
          new VecBigExpression.Zip(
              VecBigExpression.of(7, 8), VecBigExpression.of(1, 2), op);
      assertEquals(want[2 * op.ordinal()], hex.formatHex(SetExpressionCodec.encodeBigVector(zip)));
      assertEquals(zip, SetExpressionCodec.decodeBigVector(SetExpressionCodec.encodeBigVector(zip)));

      VecBigExpression scale =
          new VecBigExpression.Scale(
              new VecBigExpression.MapBig(
                  VecSetExpression.view(SetExpression.key(9), ViewSpec.interleaved(3)),
                  new BigExpression.ReadUint(SetExpression.hole(), 128)),
              BigExpression.of(3),
              op);
      assertEquals(
          want[2 * op.ordinal() + 1], hex.formatHex(SetExpressionCodec.encodeBigVector(scale)));
      assertEquals(
          scale, SetExpressionCodec.decodeBigVector(SetExpressionCodec.encodeBigVector(scale)));
    }
  }

  /** A zip of unequal arities is refused where it is built, not where it is evaluated. */
  @Test
  void aZipOfUnequalAritiesIsRefused() {
    assertThrows(
        IllegalArgumentException.class,
        () ->
            new VecBigExpression.Zip(
                VecBigExpression.of(1, 2), VecBigExpression.of(3), BigBinOp.ADD));
  }

  /** {@code map} builds a vector of big integers and {@code fold} collapses one. */
  @Test
  void bigVectorIsBuiltByMapAndCollapsedByFold() {
    VecBigExpression vector =
        new VecBigExpression.MapBig(
            VecSetExpression.view(SetExpression.key(9), ViewSpec.interleaved(3)),
            new BigExpression.ReadUint(SetExpression.hole(), 128));
    assertEquals(
        vector, SetExpressionCodec.decodeBigVector(SetExpressionCodec.encodeBigVector(vector)));

    for (BigFoldOp op : BigFoldOp.values()) {
      BigExpression fold = new BigExpression.Fold(vector, op);
      assertEquals(fold, SetExpressionCodec.decodeBig(SetExpressionCodec.encodeBig(fold)));
    }

    // Only a product fold grows with the arity: a sum costs a handful of bits.
    assertEquals(128 + 2, new BigExpression.Fold(vector, BigFoldOp.ADD).widthBound());
    assertEquals(128 * 3, new BigExpression.Fold(vector, BigFoldOp.MUL).widthBound());
    assertEquals(128, new BigExpression.Fold(vector, BigFoldOp.MIN).widthBound());
  }

  /** One value, one encoding, or a shared byte vector states nothing. */
  @Test
  void aNegativeZeroIsUnrepresentableAndATrailingZeroByteIsRefused() {
    byte[] zero = SetExpressionCodec.encodeBig(new BigExpression.Literal(BigInteger.ZERO));
    assertEquals("59534e580100180000000000", HexFormat.of().formatHex(zero));

    byte[] negativeZero = zero.clone();
    negativeZero[7] = 1;
    assertThrows(
        IllegalArgumentException.class, () -> SetExpressionCodec.decodeBig(negativeZero));
  }

  /**
   * Two bounded factors have an unbounded product: the constituent cap and the per-value width
   * bound each cap one and say nothing about their product.
   */
  @Test
  void theResultBoundRefusesAWideVectorOfWideValues() {
    assertThrows(
        IllegalArgumentException.class,
        () ->
            new VecBigExpression.MapBig(
                VecSetExpression.view(SetExpression.key(1), ViewSpec.interleaved(4096)),
                new BigExpression.ReadUint(
                    SetExpression.hole(), (int) BigExpression.MAX_VALUE_BITS)));
  }

  @Test
  void aZeroWidthReadIsRefused() {
    assertThrows(
        IllegalArgumentException.class,
        () -> new BigExpression.ReadUint(SetExpression.key(1), 0));
  }

  /**
   * Mirrors {@code the_pow_mod_wire_vector_is_stable} in the Rust crate: operand order is the one
   * thing a reader cannot infer from the bytes.
   */
  @Test
  void powModWireVectorMatchesTheRustCrate() {
    BigExpression e =
        new BigExpression.PowMod(
            new BigExpression.Literal(BigInteger.TWO),
            new BigExpression.Literal(BigInteger.TEN),
            new BigExpression.Literal(BigInteger.valueOf(1000)));
    byte[] encoded = SetExpressionCodec.encodeBig(e);
    assertEquals(
        "59534e58010027180001000000021800010000000a180002000000e803",
        HexFormat.of().formatHex(encoded));
    assertEquals(e, SetExpressionCodec.decodeBig(encoded));
  }

  /**
   * The amplification the work bound exists for, and the one the width bound structurally cannot
   * see: a residue is only as wide as its modulus.
   */
  @Test
  void aCostlyExponentiationIsRefusedWhereItsWidthIsUnremarkable() {
    BigExpression wide =
        new BigExpression.ReadUint(SetExpression.key(1), (int) BigExpression.MAX_VALUE_BITS);
    BigExpression e =
        new BigExpression.PowMod(
            new BigExpression.Literal(BigInteger.TWO), wide, wide);

    assertEquals(BigExpression.MAX_VALUE_BITS, e.widthBound());
    assertTrue(e.workBound() > BigExpression.MAX_WORK);

    byte[] encoded = SetExpressionCodec.encodeBig(e);
    assertThrows(IllegalArgumentException.class, () -> SetExpressionCodec.decodeBig(encoded));
  }

  /** The sizes a caller plausibly means are admitted, so the bound is calibrated. */
  @Test
  void rsaScaleExponentiationIsAdmitted() {
    for (int bits : new int[] {2048, 4096}) {
      BigExpression operand = new BigExpression.ReadUint(SetExpression.key(1), bits);
      BigExpression e =
          new BigExpression.PowMod(
              new BigExpression.Literal(BigInteger.TWO), operand, operand);
      assertTrue(e.workBound() <= BigExpression.MAX_WORK);
      assertEquals(e, SetExpressionCodec.decodeBig(SetExpressionCodec.encodeBig(e)));
    }
  }

  /**
   * {@code BigExpression.of} and {@code VecBigExpression.of} are the Java spelling of the query
   * language's {@code big( .. )} and {@code big( [ .. ] )}. A record cannot be overloaded on
   * shape, so the vector half takes its own home.
   */
  @Test
  void bigFactoriesMirrorTheQueryLanguage() {
    assertEquals(new BigExpression.Literal(BigInteger.valueOf(7)), BigExpression.of(7));

    VecBigExpression vector = VecBigExpression.of(1, 2, 3);
    assertEquals(3, vector.arity());

    BigExpression folded = new BigExpression.Fold(vector, BigFoldOp.ADD);
    assertEquals(folded, SetExpressionCodec.decodeBig(SetExpressionCodec.encodeBig(folded)));
  }
}
