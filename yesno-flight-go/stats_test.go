package yesnodb

import (
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
	extended := append(append([]byte(nil), canonicalStats...), 0x30, 0x63)
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
