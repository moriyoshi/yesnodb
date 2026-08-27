package yesnodb

import (
	"fmt"

	"google.golang.org/protobuf/encoding/protowire"
)

// ServerStats contains the space and reader counters returned by the stats action.
type ServerStats struct {
	AllocatedBytes uint64
	DeferredBytes  uint64
	WALBytes       uint64
	LiveReaders    uint64
	Shards         uint64
}

// DecodeServerStats decodes the public yesno.flight.v1.ServerStats protobuf.
// Unknown fields are skipped according to protobuf compatibility rules.
func DecodeServerStats(payload []byte) (ServerStats, error) {
	var stats ServerStats
	for len(payload) > 0 {
		number, wireType, consumed := protowire.ConsumeTag(payload)
		if consumed < 0 {
			return ServerStats{}, protowire.ParseError(consumed)
		}
		payload = payload[consumed:]
		if number >= 1 && number <= 5 {
			if wireType != protowire.VarintType {
				return ServerStats{}, fmt.Errorf("stats field %d is not a uint64", number)
			}
			value, size := protowire.ConsumeVarint(payload)
			if size < 0 {
				return ServerStats{}, protowire.ParseError(size)
			}
			payload = payload[size:]
			switch number {
			case 1:
				stats.AllocatedBytes = value
			case 2:
				stats.DeferredBytes = value
			case 3:
				stats.WALBytes = value
			case 4:
				stats.LiveReaders = value
			case 5:
				stats.Shards = value
			}
			continue
		}
		size := protowire.ConsumeFieldValue(number, wireType, payload)
		if size < 0 {
			return ServerStats{}, protowire.ParseError(size)
		}
		payload = payload[size:]
	}
	return stats, nil
}
