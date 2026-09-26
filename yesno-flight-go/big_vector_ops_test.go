package yesnodb

import (
	"encoding/hex"
	"testing"
)

// The reference bytes come from the Rust encoder in yesno-wire, which is the
// authority on the format. A client that only round-trips through itself can
// be self-consistently wrong, so these are compared against bytes produced
// there rather than against this package's own decoder.
func TestBigZipAndScaleMatchTheRustEncoder(t *testing.T) {
	want := []string{
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
	}
	for i, op := range []BigBinOp{BigBinAdd, BigBinSub, BigBinMul, BigBinDiv, BigBinRem} {
		z := BigZipExpr{
			Left:  BigListExpr{Elements: []BigExpr{BigLitFromInt64(7), BigLitFromInt64(8)}},
			Right: BigListExpr{Elements: []BigExpr{BigLitFromInt64(1), BigLitFromInt64(2)}},
			Op:    op,
		}
		raw, err := EncodeBigVector(z)
		if err != nil {
			t.Fatalf("zip %d: %v", op, err)
		}
		if got := hex.EncodeToString(raw); got != want[2*i] {
			t.Errorf("zip %d:\n rust %s\n go   %s", op, want[2*i], got)
		}

		sc := BigScaleExpr{
			Vector: MapBigExpr{
				Vector: ViewExpr{Input: KeyExpr{Key: 9}, View: InterleavedView(3)},
				Body:   BigReadExpr{Input: HoleExpr{}, WidthBits: 128},
			},
			Scalar: BigLitFromInt64(3),
			Op:     op,
		}
		raw, err = EncodeBigVector(sc)
		if err != nil {
			t.Fatalf("scale %d: %v", op, err)
		}
		if got := hex.EncodeToString(raw); got != want[2*i+1] {
			t.Errorf("scale %d:\n rust %s\n go   %s", op, want[2*i+1], got)
		}

		// And each decodes back to what it encoded.
		for _, e := range []VecBigExpr{z, sc} {
			raw, err := EncodeBigVector(e)
			if err != nil {
				t.Fatal(err)
			}
			back, err := DecodeBigVector(raw)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if back.Arity() != e.Arity() || back.ElementBound() != e.ElementBound() {
				t.Errorf("round trip changed the shape: %#v", back)
			}
		}
	}
}

func TestAZipOfUnequalAritiesIsRefusedAtBothEnds(t *testing.T) {
	two := BigListExpr{Elements: []BigExpr{BigLitFromInt64(1), BigLitFromInt64(2)}}
	one := BigListExpr{Elements: []BigExpr{BigLitFromInt64(3)}}

	// The encoder refuses it, so these bytes never reach a server.
	if _, err := EncodeBigVector(BigZipExpr{Left: two, Right: one, Op: BigBinAdd}); err == nil {
		t.Error("encoding a zip of unequal arities must fail")
	}

	// The decoder must refuse it too, because a peer is not obliged to have
	// used this encoder. Spliced by hand for exactly that reason.
	const header = 6
	rawTwo, err := EncodeBigVector(two)
	if err != nil {
		t.Fatal(err)
	}
	rawOne, err := EncodeBigVector(one)
	if err != nil {
		t.Fatal(err)
	}
	payload := append([]byte{}, rawTwo[:header]...)
	payload = append(payload, 40, byte(BigBinAdd)) // tagBigZip
	payload = append(payload, rawTwo[header:]...)
	payload = append(payload, rawOne[header:]...)
	if _, err := DecodeBigVector(payload); err == nil {
		t.Error("a zip of unequal arities must be refused, not evaluated")
	}
}
