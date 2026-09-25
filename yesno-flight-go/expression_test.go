package yesnodb

import (
	"encoding/hex"
	"math"
	"reflect"
	"strings"
	"testing"
)

func TestFixedExpressionVectors(t *testing.T) {
	t.Parallel()
	literal, err := Literal(0, 2, 65_536, math.MaxUint64-1)
	if err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name       string
		expression Expr
		hex        string
	}{
		{"empty", Empty(), "59534e58010000"},
		{"key", Key(42), "59534e580100012a00000000000000"},
		{"range", Range(1, 3), "59534e5801000201000000000000000300000000000000"},
		{"literal", literal, "59534e5801000904000000000000000000000002000000000000000000010000000000feffffffffffffff"},
		{
			"junction",
			And(Key(1), Or(Key(2), Key(3))),
			"59534e580100030200010100000000000000040200010200000000000000010300000000000000",
		},
		// Mirrors `the_cross_implementation_wire_vector_is_stable` in the Rust
		// crate byte for byte. Five implementations of one format drift
		// silently otherwise -- each round-trips against itself while
		// disagreeing with the others, and only a shared constant catches it.
		{
			"at-of-view",
			At(View(Key(9), InterleavedView(3)), 1),
			"59534e580100060c0300000000000000000000000001090000000000000001000000",
		},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			want, err := hex.DecodeString(test.hex)
			if err != nil {
				t.Fatal(err)
			}
			got, err := EncodeExpression(test.expression)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got, want) {
				t.Fatalf("wire mismatch\n got %x\nwant %x", got, want)
			}
			decoded, err := DecodeExpression(want)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(decoded, test.expression) {
				t.Fatalf("round trip mismatch\n got %#v\nwant %#v", decoded, test.expression)
			}
		})
	}
}

func TestEveryExpressionVariantRoundTrips(t *testing.T) {
	t.Parallel()
	literal, err := Literal(9, 1, 9)
	if err != nil {
		t.Fatal(err)
	}
	expression := And(
		Key(42),
		Or(Range(0, 10), Range(100, math.MaxUint64)),
		AndNot(Key(7), Empty()),
		At(View(Key(50), InterleavedView(3)), 1),
		// The composition the old leaf nodes could not express: the folded
		// operand is computed rather than a bare key.
		Fold(View(AndNot(Key(60), Key(61)), BlockedView(3, 65_536)), FoldOr),
		Expand(Key(70), InterleavedView(2)),
		Pack(List(Key(80), Key(81)), InterleavedView(2)),
		literal,
	)
	wire, err := EncodeExpression(expression)
	if err != nil {
		t.Fatal(err)
	}
	decoded, err := DecodeExpression(wire)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, expression) {
		t.Fatalf("round trip mismatch\n got %#v\nwant %#v", decoded, expression)
	}
}

func TestLiteralNormalizesAndRejectsReservedOrdinal(t *testing.T) {
	t.Parallel()
	literal, err := Literal(9, 1, 9, 5)
	if err != nil {
		t.Fatal(err)
	}
	if want := (LiteralExpr{Ordinals: []uint64{1, 5, 9}}); !reflect.DeepEqual(literal, want) {
		t.Fatalf("literal = %#v, want %#v", literal, want)
	}
	if _, err := Literal(math.MaxUint64); err == nil {
		t.Fatal("reserved ordinal was accepted")
	}
}

func TestDerivedBooleanOperatorsUseV1Nodes(t *testing.T) {
	t.Parallel()
	left, right := Key(1), Key(2)
	if want := AndNot(Or(left, right), And(left, right)); !reflect.DeepEqual(Xor(left, right), want) {
		t.Fatalf("Xor did not lower to v1 nodes")
	}
	if want := AndNot(Range(0, math.MaxUint64), left); !reflect.DeepEqual(Complement(left), want) {
		t.Fatalf("Complement did not lower to v1 nodes")
	}
}

func TestMagicCollidingBareKeyIsNotAnExpression(t *testing.T) {
	t.Parallel()
	if LooksLikeExpression([]byte{'Y', 'S', 'N', 'X', 0, 0, 0, 0}) {
		t.Fatal("bare eight-byte key was mistaken for an expression")
	}
	wire, err := EncodeExpression(Key(0x00000000584e5359))
	if err != nil {
		t.Fatal(err)
	}
	if !LooksLikeExpression(wire) {
		t.Fatal("encoded expression was not recognized")
	}
}

func TestExpressionBoundsAndMalformedPayloads(t *testing.T) {
	t.Parallel()
	deep := Empty()
	for range MaxExpressionDepth + 2 {
		deep = And(deep)
	}
	if _, err := EncodeExpression(deep); err == nil {
		t.Fatal("overly deep expression encoded")
	}
	many := make([]Expr, MaxExpressionNodes)
	for index := range many {
		many[index] = Empty()
	}
	if _, err := EncodeExpression(Or(many...)); err == nil {
		t.Fatal("oversized expression encoded")
	}
	for _, payload := range [][]byte{
		nil,
		[]byte("YSNX\x01\x00\x03\x00\x00"),
		append([]byte("YSNX\x01\x00\x00"), []byte("extra")...),
		[]byte("YSNX\x01\x01\x00"),
		[]byte("YSNX\x01\x00\x09\x02\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00"),
	} {
		if _, err := DecodeExpression(payload); err == nil {
			t.Fatalf("malformed payload decoded: %x", payload)
		}
	}
}

func TestViewOrdinalMappingRefusesOverflowAndReservedOrdinal(t *testing.T) {
	t.Parallel()
	if ordinal, ok := InterleavedView(3).OrdinalOf(2, 5); !ok || ordinal != 17 {
		t.Fatalf("interleaved mapping = %d, %v", ordinal, ok)
	}
	if ordinal, ok := BlockedView(3, 100).OrdinalOf(2, 5); !ok || ordinal != 205 {
		t.Fatalf("blocked mapping = %d, %v", ordinal, ok)
	}
	if _, ok := InterleavedView(1).OrdinalOf(0, math.MaxUint64); ok {
		t.Fatal("reserved ordinal was addressable")
	}
	if _, ok := BlockedView(math.MaxUint32, math.MaxUint64).OrdinalOf(1, 0); ok {
		t.Fatal("overflowing blocked ordinal was addressable")
	}
}

func TestFixedQueryRequestVector(t *testing.T) {
	t.Parallel()
	want, err := hex.DecodeString("59534e510101070000000000000059534e580100012a00000000000000")
	if err != nil {
		t.Fatal(err)
	}
	request := QueryAt(Key(42), 7)
	got, err := request.Encode()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("query wire mismatch\n got %x\nwant %x", got, want)
	}
	decoded, err := DecodeQueryRequest(want)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.Version == nil || *decoded.Version != 7 || !reflect.DeepEqual(decoded.Expression, Key(42)) {
		t.Fatalf("decoded query mismatch: %#v", decoded)
	}
}

func FuzzDecodeExpression(f *testing.F) {
	f.Add([]byte("YSNX\x01\x00\x00"))
	f.Add([]byte("YSNX\x01\x00\x01\x2a\x00\x00\x00\x00\x00\x00\x00"))
	f.Fuzz(func(t *testing.T, payload []byte) {
		_, _ = DecodeExpression(payload)
	})
}

func FuzzDecodeQueryRequest(f *testing.F) {
	wire, err := QueryAt(Key(42), 7).Encode()
	if err != nil {
		f.Fatal(err)
	}
	f.Add(wire)
	f.Fuzz(func(t *testing.T, payload []byte) {
		_, _ = DecodeQueryRequest(payload)
	})
}

// TestTheFacetQueryRoundTrips mirrors the Rust, Python and Java tests of the
// same name. It is a map, not a fold: it applies a query to each constituent
// rather than combining them, and its result is the row marginal.
func TestTheFacetQueryRoundTrips(t *testing.T) {
	t.Parallel()
	facet := MapInt(
		View(Key(9), InterleavedView(4)),
		Cardinality(And(Hole(), Key(7))),
	)
	wire, err := EncodeIntVector(facet)
	if err != nil {
		t.Fatal(err)
	}
	decoded, err := DecodeIntVector(wire)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, facet) {
		t.Fatalf("round trip mismatch:\n got %#v\nwant %#v", decoded, facet)
	}
	if got := facet.Arity(); got != 4 {
		t.Fatalf("arity %d, want 4 -- a map preserves shape", got)
	}
	// An integer vector is not a set, and the error names both sides.
	if _, err := DecodeExpression(wire); err == nil {
		t.Fatal("an integer vector decoded as a set")
	} else if !strings.Contains(err.Error(), "where a set was required") {
		t.Fatalf("unexpected error: %v", err)
	}
}

// TestTheHoleIsScopedToAMapBody pins both halves: a hole outside a body is
// refused, and a map inside a body is refused while a map in a vector position
// -- sequential rather than nested -- still decodes.
func TestTheHoleIsScopedToAMapBody(t *testing.T) {
	t.Parallel()
	if _, err := EncodeExpression(Hole()); err != nil {
		t.Fatal(err)
	}
	wire, _ := EncodeExpression(Hole())
	if _, err := DecodeExpression(wire); err == nil {
		t.Fatal("a bare hole decoded")
	} else if !strings.Contains(err.Error(), "outside a map body") {
		t.Fatalf("unexpected error: %v", err)
	}

	two := List(Key(1), Key(2))
	nested := MapSet(two, Fold(MapSet(two, Hole()), FoldOr))
	wire, err := EncodeExpression(Fold(nested, FoldOr))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := DecodeExpression(wire); err == nil {
		t.Fatal("a nested map decoded")
	} else if !strings.Contains(err.Error(), "may not contain a map") {
		t.Fatalf("unexpected error: %v", err)
	}

	sequential := Fold(MapSet(MapSet(two, Hole()), Hole()), FoldOr)
	wire, err = EncodeExpression(sequential)
	if err != nil {
		t.Fatal(err)
	}
	got, err := DecodeExpression(wire)
	if err != nil {
		t.Fatalf("a map in a vector position must decode: %v", err)
	}
	if !reflect.DeepEqual(got, sequential) {
		t.Fatal("round trip mismatch for a sequential map")
	}
}

// ---------------------------------------------------------------------------
// The big-integer sort
// ---------------------------------------------------------------------------

// Mirrors `the_big_cross_implementation_wire_vector_is_stable` in the Rust
// crate and its Python counterpart. Five implementations of one format drift
// silently otherwise: each can round-trip against itself while disagreeing
// with the others.
func TestBigWireVectorMatchesTheRustCrate(t *testing.T) {
	e := BigSaturateExpr{
		Input: BigMulExpr{
			Left:  BigReadExpr{Input: KeyExpr{Key: 4}, WidthBits: 128},
			Right: BigLitFromInt64(-3),
		},
		Bits: 32,
	}
	encoded, err := EncodeBig(e)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	const want = "59534e58010023200000001f1a8000000001040000000000000018010100000003"
	if got := hex.EncodeToString(encoded); got != want {
		t.Fatalf("wire vector drifted:\n got %s\nwant %s", got, want)
	}
	back, err := DecodeBig(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if !reflect.DeepEqual(back, e) {
		t.Fatalf("round trip: got %#v", back)
	}
}

func TestBigNodesRoundTrip(t *testing.T) {
	cases := []BigExpr{
		BigLitFromInt64(0),
		BigLitFromInt64(1),
		BigLitFromInt64(-1),
		BigLitFromInt64(1 << 40),
		BigLitFromInt64(-(1 << 40)),
		BigWidenExpr{Input: CardinalityExpr{Input: KeyExpr{Key: 7}}},
		BigReadExpr{Input: KeyExpr{Key: 4}, WidthBits: 128},
		BigReadSignedExpr{Input: KeyExpr{Key: 4}, WidthBits: 8},
		BigNegExpr{Input: BigLitFromInt64(5)},
		BigAddExpr{Left: BigLitFromInt64(1), Right: BigLitFromInt64(2)},
		BigSubExpr{Left: BigLitFromInt64(1), Right: BigLitFromInt64(2)},
		BigMulExpr{Left: BigLitFromInt64(3), Right: BigLitFromInt64(4)},
		BigDivExpr{Left: BigLitFromInt64(7), Right: BigLitFromInt64(2)},
		BigRemExpr{Left: BigLitFromInt64(7), Right: BigLitFromInt64(2)},
		BigTruncateExpr{Input: BigLitFromInt64(300), Bits: 8},
		BigSaturateExpr{Input: BigLitFromInt64(300), Bits: 8},
	}
	for _, e := range cases {
		encoded, err := EncodeBig(e)
		if err != nil {
			t.Fatalf("encode %#v: %v", e, err)
		}
		back, err := DecodeBig(encoded)
		if err != nil {
			t.Fatalf("decode %#v: %v", e, err)
		}
		if !reflect.DeepEqual(back, e) {
			t.Fatalf("round trip %#v -> %#v", e, back)
		}
	}
}

// The amplification the width budget exists to refuse. A read is six bytes
// that declare a width, so a product of two maximal reads is a tiny payload
// describing a value no server should try to build.
func TestBigWidthBudgetRefusesAnAmplification(t *testing.T) {
	wide := BigReadExpr{Input: KeyExpr{Key: 1}, WidthBits: MaxValueBits}
	if _, err := EncodeBig(BigMulExpr{Left: wide, Right: wide}); err == nil {
		t.Fatal("a product of two maximal reads must be refused")
	}
	// One bit under the bound is accepted, so the refusal above is the bound
	// working rather than the shape being rejected.
	half := BigReadExpr{Input: KeyExpr{Key: 1}, WidthBits: MaxValueBits / 2}
	if _, err := EncodeBig(BigMulExpr{Left: half, Right: half}); err != nil {
		t.Fatalf("a product just inside the bound must be accepted: %v", err)
	}
}

// One value, one encoding, or a shared byte vector states nothing.
func TestBigLiteralCanonicalityIsEnforced(t *testing.T) {
	if _, err := EncodeBig(BigLitExpr{Magnitude: []byte{1, 0}}); err == nil {
		t.Fatal("a trailing zero byte must be refused rather than trimmed")
	}
	if _, err := EncodeBig(BigLitExpr{Negative: true}); err == nil {
		t.Fatal("a negative zero must be unrepresentable")
	}
	if BigLitFromInt64(0).Negative {
		t.Fatal("zero is never negative")
	}
}

func TestBigZeroWidthReadIsRefused(t *testing.T) {
	if _, err := EncodeBig(BigReadExpr{Input: KeyExpr{Key: 1}}); err == nil {
		t.Fatal("a read of zero bits denotes nothing and must be refused")
	}
}

func TestABigTagWhereASetBelongsIsASortMismatch(t *testing.T) {
	encoded, err := EncodeBig(BigLitFromInt64(1))
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	if _, err := DecodeExpression(encoded); err == nil {
		t.Fatal("a big-integer payload must not decode as a set")
	}
}

func TestBigVectorIsBuiltByMapAndCollapsedByFold(t *testing.T) {
	vector := MapBigExpr{
		Vector: ViewExpr{Input: KeyExpr{Key: 9}, View: ViewSpec{Sets: 3, Layout: ViewInterleaved}},
		Body:   BigReadExpr{Input: HoleExpr{}, WidthBits: 128},
	}
	encoded, err := EncodeBigVector(vector)
	if err != nil {
		t.Fatalf("encode vector: %v", err)
	}
	back, err := DecodeBigVector(encoded)
	if err != nil {
		t.Fatalf("decode vector: %v", err)
	}
	if !reflect.DeepEqual(back, vector) {
		t.Fatalf("vector round trip: %#v", back)
	}

	for _, op := range []BigFoldOp{BigFoldAdd, BigFoldMul, BigFoldMin, BigFoldMax} {
		f := BigFoldExpr{Vector: vector, Op: op}
		enc, err := EncodeBig(f)
		if err != nil {
			t.Fatalf("encode fold %d: %v", op, err)
		}
		got, err := DecodeBig(enc)
		if err != nil {
			t.Fatalf("decode fold %d: %v", op, err)
		}
		if !reflect.DeepEqual(got, f) {
			t.Fatalf("fold round trip %d: %#v", op, got)
		}
	}
}

// Only a product fold grows with the arity: a sum costs a handful of bits.
func TestOnlyAProductFoldGrowsWithTheArity(t *testing.T) {
	vector := MapBigExpr{
		Vector: ViewExpr{Input: KeyExpr{Key: 9}, View: ViewSpec{Sets: 3, Layout: ViewInterleaved}},
		Body:   BigReadExpr{Input: HoleExpr{}, WidthBits: 128},
	}
	for _, c := range []struct {
		op   BigFoldOp
		want uint64
	}{
		{BigFoldAdd, 128 + 2},
		{BigFoldMul, 128 * 3},
		{BigFoldMin, 128},
		{BigFoldMax, 128},
	} {
		if got := (BigFoldExpr{Vector: vector, Op: c.op}).WidthBound(); got != c.want {
			t.Fatalf("op %d: got %d, want %d", c.op, got, c.want)
		}
	}
}

// Two bounded factors have an unbounded product: the constituent cap and the
// per-value width bound each cap one and say nothing about their product.
func TestBigVectorResultBoundRefusesAWideVectorOfWideValues(t *testing.T) {
	wide := MapBigExpr{
		Vector: ViewExpr{Input: KeyExpr{Key: 1}, View: ViewSpec{Sets: 4096, Layout: ViewInterleaved}},
		Body:   BigReadExpr{Input: HoleExpr{}, WidthBits: MaxValueBits},
	}
	if _, err := EncodeBigVector(wide); err == nil {
		t.Fatal("a wide vector of wide values must be refused")
	}
	ok := MapBigExpr{
		Vector: ViewExpr{Input: KeyExpr{Key: 1}, View: ViewSpec{Sets: 64, Layout: ViewInterleaved}},
		Body:   BigReadExpr{Input: HoleExpr{}, WidthBits: 1024},
	}
	if _, err := EncodeBigVector(ok); err != nil {
		t.Fatalf("a vector inside the bound must be accepted: %v", err)
	}
}

// Mirrors `the_pow_mod_wire_vector_is_stable` in the Rust crate: the operand
// order is the one thing a reader cannot infer from the bytes.
func TestPowModWireVectorMatchesTheRustCrate(t *testing.T) {
	e := BigPowModExpr{
		Base:    BigLitFromInt64(2),
		Exp:     BigLitFromInt64(10),
		Modulus: BigLitFromInt64(1000),
	}
	encoded, err := EncodeBig(e)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	const want = "59534e58010027180001000000021800010000000a180002000000e803"
	if got := hex.EncodeToString(encoded); got != want {
		t.Fatalf("wire vector drifted:\n got %s\nwant %s", got, want)
	}
	back, err := DecodeBig(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if !reflect.DeepEqual(back, e) {
		t.Fatalf("round trip: %#v", back)
	}
}

// The amplification the work bound exists for, and the one the width bound
// structurally cannot see: a residue is only as wide as its modulus.
func TestACostlyExponentiationIsRefusedWhereItsWidthIsUnremarkable(t *testing.T) {
	wide := func(bits uint32) BigExpr {
		return BigReadExpr{Input: KeyExpr{Key: 1}, WidthBits: bits}
	}
	e := BigPowModExpr{Base: BigLitFromInt64(2), Exp: wide(1 << 20), Modulus: wide(1 << 20)}

	if e.WidthBound() != 1<<20 || e.WidthBound() > MaxValueBits {
		t.Fatalf("the width should be unremarkable, got %d", e.WidthBound())
	}
	if e.WorkBound() <= MaxWork {
		t.Fatalf("the work should exceed the budget, got %d", e.WorkBound())
	}
	if _, err := EncodeBig(e); err == nil {
		t.Fatal("a costly exponentiation must be refused")
	}
}

// And the sizes a caller plausibly means are admitted, so the bound is
// calibrated rather than merely restrictive.
func TestRsaScaleExponentiationIsAdmitted(t *testing.T) {
	for _, bits := range []uint32{2048, 4096} {
		operand := BigReadExpr{Input: KeyExpr{Key: 1}, WidthBits: bits}
		e := BigPowModExpr{Base: BigLitFromInt64(2), Exp: operand, Modulus: operand}
		if e.WorkBound() > MaxWork {
			t.Fatalf("%d-bit modulus cost %d exceeds the budget", bits, e.WorkBound())
		}
		if _, err := EncodeBig(e); err != nil {
			t.Fatalf("%d-bit modulus must be admitted: %v", bits, err)
		}
	}
}

// Big and BigVec are the Go spelling of the query language's big( .. ) and
// big( [ .. ] ). Go cannot overload, so the vector half takes its own name.
func TestBigAndBigVecMirrorTheQueryLanguage(t *testing.T) {
	if got := Big(7); !reflect.DeepEqual(got, BigLitFromInt64(7)) {
		t.Fatalf("Big(7) = %#v", got)
	}
	want := BigListExpr{Elements: []BigExpr{
		BigLitFromInt64(1), BigLitFromInt64(2), BigLitFromInt64(3),
	}}
	if got := BigVec(1, 2, 3); !reflect.DeepEqual(got, want) {
		t.Fatalf("BigVec(1,2,3) = %#v", got)
	}

	folded := BigFoldExpr{Vector: BigVec(1, 2, 3), Op: BigFoldAdd}
	encoded, err := EncodeBig(folded)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	back, err := DecodeBig(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if !reflect.DeepEqual(back, folded) {
		t.Fatalf("round trip: %#v", back)
	}
}
