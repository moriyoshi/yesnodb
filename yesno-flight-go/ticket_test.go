package yesnodb

import (
	"encoding/binary"
	"encoding/hex"
	"reflect"
	"testing"
)

func TestFixedTicketHeaderAndExpressionSuffix(t *testing.T) {
	t.Parallel()
	ticket := Ticket{
		Version:        9,
		Key:            42,
		PrefixLo:       1,
		PrefixHi:       1 << 20,
		ExpressionHash: 7,
		Expression:     And(Key(1), Range(2, 9)),
	}
	wire, err := ticket.Encode()
	if err != nil {
		t.Fatal(err)
	}
	// Mirrors `the_cross_implementation_ticket_header_is_stable` in the Rust
	// crate byte for byte: version(8) key(8) prefix_lo(8) prefix_hi(8)
	// expr_hash(8) wire(8). The length alone is not enough -- this client was
	// self-consistently wrong about it for a week while every test here passed.
	wantHeader, err := hex.DecodeString("09000000000000002a000000000000000100000000000000000010000000000007000000000000000000000000000000")
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(wire[:TicketHeaderLength], wantHeader) {
		t.Fatalf("ticket header mismatch\n got %x\nwant %x", wire[:TicketHeaderLength], wantHeader)
	}
	decoded, err := DecodeTicket(wire)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, ticket) {
		t.Fatalf("ticket round trip mismatch\n got %#v\nwant %#v", decoded, ticket)
	}
}

// The representation lives in the header's sixth word, so its position is
// checked and not merely its presence: a vector for the default alone would
// still pass if the field moved, because its bytes are zero.
func TestRequestedRepresentationIsTheSixthHeaderWord(t *testing.T) {
	t.Parallel()
	ticket := Ticket{
		Version:        9,
		Key:            42,
		PrefixLo:       1,
		PrefixHi:       1 << 20,
		ExpressionHash: 7,
		Wire:           SetWireContainers,
	}
	wire, err := ticket.Encode()
	if err != nil {
		t.Fatal(err)
	}
	want := "09000000000000002a000000000000000100000000000000000010000000000007000000000000000100000000000000"
	if got := hex.EncodeToString(wire); got != want {
		t.Fatalf("container ticket mismatch\n got %s\nwant %s", got, want)
	}
	decoded, err := DecodeTicket(wire)
	if err != nil || decoded.Wire != SetWireContainers {
		t.Fatalf("decoded = %#v, %v", decoded, err)
	}
}

// An unrecognised representation rejects the ticket rather than defaulting:
// reading a stream in a representation the server did not promise is
// corruption, and indistinguishable from a server that understood.
func TestUnknownRepresentationIsRejected(t *testing.T) {
	t.Parallel()
	wire, err := (Ticket{Version: 1, Key: 2, PrefixHi: 1 << 48}).Encode()
	if err != nil {
		t.Fatal(err)
	}
	binary.LittleEndian.PutUint64(wire[40:48], 200)
	if _, err := DecodeTicket(wire); err == nil {
		t.Fatal("ticket with an unknown representation decoded")
	}
	if _, err := (Ticket{Wire: SetWire(200)}).Encode(); err == nil {
		t.Fatal("ticket with an unknown representation encoded")
	}
	// And the three it knows still round-trip, so the check is not vacuous.
	for _, want := range []SetWire{SetWireOrdinals, SetWireContainers, SetWireBitvector} {
		encoded, err := (Ticket{Version: 1, Key: 2, PrefixHi: 1 << 48, Wire: want}).Encode()
		if err != nil {
			t.Fatal(err)
		}
		decoded, err := DecodeTicket(encoded)
		if err != nil || decoded.Wire != want {
			t.Fatalf("%s did not round trip: %#v, %v", want, decoded, err)
		}
	}
}

func TestHeaderOnlyTicketRemainsValid(t *testing.T) {
	t.Parallel()
	ticket := Ticket{Version: 3, Key: 7, PrefixHi: 1 << 48}
	wire, err := ticket.Encode()
	if err != nil {
		t.Fatal(err)
	}
	if len(wire) != TicketHeaderLength {
		t.Fatalf("header-only ticket has %d bytes", len(wire))
	}
	decoded, err := DecodeTicket(wire)
	if err != nil || !reflect.DeepEqual(decoded, ticket) {
		t.Fatalf("header-only decode = %#v, %v", decoded, err)
	}
}

func TestMalformedTicketsAreRejected(t *testing.T) {
	t.Parallel()
	if _, err := DecodeTicket(make([]byte, TicketHeaderLength-1)); err == nil {
		t.Fatal("short ticket decoded")
	}
	if _, err := (Ticket{PrefixLo: 10, PrefixHi: 5}).Encode(); err == nil {
		t.Fatal("inverted ticket encoded")
	}
	wire, err := (Ticket{Version: 1, Key: 2, PrefixHi: 10}).Encode()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := DecodeTicket(append(wire, []byte("not an expression")...)); err == nil {
		t.Fatal("ticket with malformed suffix decoded")
	}
}

func FuzzDecodeTicket(f *testing.F) {
	wire, err := (Ticket{Version: 1, Key: 2, PrefixHi: 1 << 48}).Encode()
	if err != nil {
		f.Fatal(err)
	}
	f.Add(wire)
	f.Fuzz(func(t *testing.T, payload []byte) {
		_, _ = DecodeTicket(payload)
	})
}
