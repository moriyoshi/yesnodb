package yesnodb

import (
	"math"
	"testing"
)

func TestPairRejectsReservedOrdinal(t *testing.T) {
	t.Parallel()
	if err := (Pair{Key: math.MaxUint64, Ordinal: math.MaxUint64 - 1}).Validate(); err != nil {
		t.Fatalf("maximum key or valid ordinal rejected: %v", err)
	}
	if err := (Pair{Key: 1, Ordinal: math.MaxUint64}).Validate(); err == nil {
		t.Fatal("reserved ordinal accepted")
	}
}
