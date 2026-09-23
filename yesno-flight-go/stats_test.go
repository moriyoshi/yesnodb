package yesnodb

import (
	"math"
	"reflect"
	"testing"
)

var canonicalStats = []byte{
	0x08, 0x80, 0x08,
	0x10, 0x40,
	0x18, 0x80, 0x01,
	0x20, 0x02,
	0x28, 0x04,
}

func TestServerStatsMatchesPublicProtoVector(t *testing.T) {
	t.Parallel()
	want := ServerStats{AllocatedBytes: 1024, DeferredBytes: 64, WALBytes: 128, LiveReaders: 2, Shards: 4}
	got, err := DecodeServerStats(canonicalStats)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("stats = %#v, want %#v", got, want)
	}
}

func TestServerStatsIgnoresUnknownFieldsAndRejectsCorruption(t *testing.T) {
	t.Parallel()
	// Field 7. This probe used to name field 6, which became Features; an
	// unknown-field test must name a field that is still unknown.
	extended := append(append([]byte(nil), canonicalStats...), 0x38, 0x63)
	if _, err := DecodeServerStats(extended); err != nil {
		t.Fatalf("unknown field rejected: %v", err)
	}
	for _, payload := range [][]byte{{0x08, 0x80}, {0x0a, 0x00}, {0x00}} {
		if _, err := DecodeServerStats(payload); err == nil {
			t.Fatalf("malformed stats decoded: %x", payload)
		}
	}
}

func FuzzDecodeServerStats(f *testing.F) {
	f.Add(canonicalStats)
	f.Fuzz(func(t *testing.T, payload []byte) {
		_, _ = DecodeServerStats(payload)
	})
}

func TestServerStatsReadsTheCapabilityField(t *testing.T) {
	t.Parallel()
	// Field 6 is Features. A server predating it reports zero, because
	// protobuf decodes an absent field as its default.
	withFeatures := append(append([]byte(nil), canonicalStats...), 0x30, 0x03)
	got, err := DecodeServerStats(withFeatures)
	if err != nil {
		t.Fatal(err)
	}
	if got.Features != FeatureMixedPut|FeatureWriteTransactions {
		t.Fatalf("features = %d, want %d", got.Features, FeatureMixedPut|FeatureWriteTransactions)
	}
	absent, err := DecodeServerStats(canonicalStats)
	if err != nil {
		t.Fatal(err)
	}
	if absent.Features != 0 {
		t.Fatalf("a server predating the field must report 0, got %d", absent.Features)
	}
}

func TestMutationConstructorsAndValidation(t *testing.T) {
	t.Parallel()
	if got := Insert(1, 2); got != (Mutation{Key: 1, Lo: 2, Hi: 2, Op: OpInsert}) {
		t.Fatalf("Insert = %#v", got)
	}
	if got := DeleteKey(7); got != (Mutation{Key: 7, Op: OpDeleteKey}) {
		t.Fatalf("DeleteKey = %#v", got)
	}
	if got := InsertRange(1, 5, 9); got != (Mutation{Key: 1, Lo: 5, Hi: 9, Op: OpInsertRange}) {
		t.Fatalf("InsertRange = %#v", got)
	}
	// A delete-key row carries no ordinals, so the reserved ceiling does not
	// apply to it.
	if err := DeleteKey(math.MaxUint64).Validate(); err != nil {
		t.Fatalf("DeleteKey must validate: %v", err)
	}
	if err := Insert(1, math.MaxUint64).Validate(); err == nil {
		t.Fatal("the reserved ordinal must be refused")
	}
	if err := InsertRange(1, 9, 5).Validate(); err == nil {
		t.Fatal("an inverted range must be refused")
	}
	if err := (Mutation{Op: 99}).Validate(); err == nil {
		t.Fatal("an unknown op must be refused")
	}
}
