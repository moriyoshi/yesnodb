package yesnodb

import (
	"encoding/binary"
	"errors"
)

// TicketHeaderLength is the fixed ticket header size: six little-endian
// uint64s, in the order Ticket declares them. A ticket may append an encoded
// expression after this header.
//
// This was 40 bytes until the set representation was added to the header on
// 2026-09-30, and this constant was not widened with it. The client kept
// round-tripping against its own 40-byte header, so every test here stayed
// green, while a real server ticket arrived with eight bytes that were handed
// to DecodeExpression and rejected as "a malformed expression". The fixed
// vector in TestFixedTicketHeaderAndExpressionSuffix is what keeps this in step
// with the Rust encoder; the length alone cannot, because the client is free to
// be self-consistently wrong about it.
const TicketHeaderLength = 48

// SetWire is how a set result is encoded in the DoGet stream.
//
// Only sets have a choice: a vector or a scalar answers one small batch whose
// shape no representation question applies to. The numbers go on the wire and
// must never be reordered.
type SetWire uint64

const (
	// SetWireOrdinals is one uint64 per ordinal. The default, because a client
	// that has not asked for containers may have no decoder for them.
	SetWireOrdinals SetWire = 0
	// SetWireContainers is container payloads, byte-identical to what the page
	// store and a .roaring file hold.
	SetWireContainers SetWire = 1
	// SetWireBitvector is a wholly materialized bitvector over the ticket's
	// prefix window, one bit per ordinal position, gaps included.
	SetWireBitvector SetWire = 2
)

// String names the representation for diagnostics.
func (w SetWire) String() string {
	switch w {
	case SetWireOrdinals:
		return "ordinals"
	case SetWireContainers:
		return "containers"
	case SetWireBitvector:
		return "bitvector"
	default:
		return "unknown"
	}
}

func (w SetWire) valid() bool {
	return w == SetWireOrdinals || w == SetWireContainers || w == SetWireBitvector
}

// Ticket identifies one snapshot and one half-open chunk-prefix range.
type Ticket struct {
	Version        uint64
	Key            uint64
	PrefixLo       uint64
	PrefixHi       uint64
	ExpressionHash uint64
	// Wire is the representation the DoGet stream will use. The zero value is
	// SetWireOrdinals, so a Ticket literal that omits it asks for ordinals.
	Wire       SetWire
	Expression Expr
}

// Encode returns the complete ticket bytes.
func (t Ticket) Encode() ([]byte, error) {
	if t.PrefixLo > t.PrefixHi {
		return nil, errors.New("ticket carries an inverted prefix range")
	}
	if !t.Wire.valid() {
		return nil, errors.New("ticket asks for an unknown set representation")
	}
	out := make([]byte, 0, TicketHeaderLength+32)
	for _, value := range [...]uint64{
		t.Version, t.Key, t.PrefixLo, t.PrefixHi, t.ExpressionHash, uint64(t.Wire),
	} {
		out = binary.LittleEndian.AppendUint64(out, value)
	}
	if t.Expression != nil {
		expression, err := EncodeExpression(t.Expression)
		if err != nil {
			return nil, err
		}
		out = append(out, expression...)
	}
	return out, nil
}

// DecodeTicket validates and decodes one server-issued ticket.
func DecodeTicket(payload []byte) (Ticket, error) {
	if len(payload) < TicketHeaderLength {
		return Ticket{}, errors.New("ticket is shorter than its 48-byte header")
	}
	read := func(index int) uint64 {
		return binary.LittleEndian.Uint64(payload[index*8 : index*8+8])
	}
	ticket := Ticket{
		Version:        read(0),
		Key:            read(1),
		PrefixLo:       read(2),
		PrefixHi:       read(3),
		ExpressionHash: read(4),
		Wire:           SetWire(read(5)),
	}
	if ticket.PrefixLo > ticket.PrefixHi {
		return Ticket{}, errors.New("ticket carries an inverted prefix range")
	}
	// An unrecognised representation is an error, never a fallback to the
	// default: reading a stream in a representation the server did not promise
	// is corruption, and indistinguishable from a server that understood.
	if !ticket.Wire.valid() {
		return Ticket{}, errors.New("ticket asks for an unknown set representation")
	}
	if len(payload) > TicketHeaderLength {
		expression, err := DecodeExpression(payload[TicketHeaderLength:])
		if err != nil {
			return Ticket{}, errors.New("ticket carries a malformed expression")
		}
		ticket.Expression = expression
	}
	return ticket, nil
}
