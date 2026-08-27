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
