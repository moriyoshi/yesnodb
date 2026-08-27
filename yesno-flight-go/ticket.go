package yesnodb

import (
	"encoding/binary"
	"errors"
)

// TicketHeaderLength is the fixed v1 ticket header size. A ticket may append
// an encoded expression after this header.
const TicketHeaderLength = 40

// Ticket identifies one snapshot and one half-open chunk-prefix range.
type Ticket struct {
	Version        uint64
	Key            uint64
	PrefixLo       uint64
	PrefixHi       uint64
	ExpressionHash uint64
	Expression     Expr
}

// Encode returns the complete ticket bytes.
func (t Ticket) Encode() ([]byte, error) {
	if t.PrefixLo > t.PrefixHi {
		return nil, errors.New("ticket carries an inverted prefix range")
	}
	out := make([]byte, 0, TicketHeaderLength+32)
	for _, value := range [...]uint64{t.Version, t.Key, t.PrefixLo, t.PrefixHi, t.ExpressionHash} {
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
		return Ticket{}, errors.New("ticket is shorter than its 40-byte header")
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
	}
	if ticket.PrefixLo > ticket.PrefixHi {
		return Ticket{}, errors.New("ticket carries an inverted prefix range")
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
