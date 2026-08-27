package yesnodb

import (
	"encoding/binary"
	"strings"
	"testing"
)

// DoPut's app_metadata carried 8 bytes -- the row count alone -- until
// 2026-09-12, when the commit version was appended. This client rejected any
// other width, so every ingest failed outright against a current server, and no
// Rust gate noticed: this package has its own gate and nothing else runs it.
func TestAckDecodesBothWireWidths(t *testing.T) {
	short := make([]byte, 8)
	binary.LittleEndian.PutUint64(short, 5000)
	ack, err := decodeAck(short)
	if err != nil {
		t.Fatalf("an 8-byte acknowledgement is an older server, not an error: %v", err)
	}
	if ack.Rows != 5000 || ack.Version != 0 {
		t.Fatalf("want rows 5000 and no version, got %+v", ack)
	}

	full := make([]byte, 16)
	binary.LittleEndian.PutUint64(full[:8], 5000)
	binary.LittleEndian.PutUint64(full[8:], 42)
	ack, err = decodeAck(full)
	if err != nil {
		t.Fatalf("a 16-byte acknowledgement must decode: %v", err)
	}
	if ack.Rows != 5000 || ack.Version != 42 {
		t.Fatalf("want rows 5000 and version 42, got %+v", ack)
	}
}

// No commit is ever assigned version 0, so a zero means the server committed
// nothing and must not reach a caller as a readable version.
func TestAckReportsAZeroVersionAsAbsent(t *testing.T) {
	empty := make([]byte, 16)
	ack, err := decodeAck(empty)
	if err != nil {
		t.Fatalf("an empty ingest is not a protocol error: %v", err)
	}
	if ack.Rows != 0 || ack.Version != 0 {
		t.Fatalf("want a zero ack, got %+v", ack)
	}
}

// Including 24, so a future widening is detected by an older client rather than
// having its extra field silently ignored.
func TestAckRefusesAnyOtherWidth(t *testing.T) {
	for _, width := range []int{0, 4, 9, 15, 24} {
		if _, err := decodeAck(make([]byte, width)); err == nil {
			t.Fatalf("a %d-byte acknowledgement must be refused", width)
		} else if !strings.Contains(err.Error(), "expected 8 or 16") {
			t.Fatalf("a %d-byte acknowledgement gave the wrong error: %v", width, err)
		}
	}
}
