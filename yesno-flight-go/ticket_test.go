package yesnodb

import (
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
	wantHeader, err := hex.DecodeString("09000000000000002a00000000000000010000000000000000001000000000000700000000000000")
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
