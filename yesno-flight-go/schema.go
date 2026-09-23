package yesnodb

import (
	"fmt"
	"math"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
)

// Pair is one key and ordinal mutation.
type Pair struct {
	Key     uint64
	Ordinal uint64
}

// Validate checks the ordinal ceiling before a pair reaches the network.
func (p Pair) Validate() error {
	if p.Ordinal == math.MaxUint64 {
		return fmt.Errorf("ordinal %d is reserved and cannot be stored", p.Ordinal)
	}
	return nil
}

// Row operations carried by MutationsSchema. These numbers go on the wire and
// are never reordered.
const (
	OpInsert      uint8 = 0
	OpRemove      uint8 = 1
	OpInsertRange uint8 = 2
	OpRemoveRange uint8 = 3
	OpDeleteKey   uint8 = 4
)

// Capability bits reported by ServerStats.Features.
//
// Check before sending a new DoPut command: a server predating the field
// reports zero and treats an unrecognised command as insert, which would turn
// a mixed batch's removals into insertions with no error anywhere.
const (
	FeatureMixedPut          uint64 = 1 << 0
	FeatureWriteTransactions uint64 = 1 << 1
)

// Mutation is one row of MutationsSchema.
//
// Build these with the constructors rather than by hand: a single-ordinal
// operation is a range whose bounds are equal, and the wire cannot detect a
// caller getting that wrong.
type Mutation struct {
	Key uint64
	Lo  uint64
	Hi  uint64
	Op  uint8
}

// Insert adds one ordinal to a key.
func Insert(key, ordinal uint64) Mutation {
	return Mutation{Key: key, Lo: ordinal, Hi: ordinal, Op: OpInsert}
}

// Remove drops one ordinal from a key.
func Remove(key, ordinal uint64) Mutation {
	return Mutation{Key: key, Lo: ordinal, Hi: ordinal, Op: OpRemove}
}

// InsertRange adds the inclusive range [lo, hi] to a key.
func InsertRange(key, lo, hi uint64) Mutation {
	return Mutation{Key: key, Lo: lo, Hi: hi, Op: OpInsertRange}
}

// RemoveRange drops the inclusive range [lo, hi] from a key.
func RemoveRange(key, lo, hi uint64) Mutation {
	return Mutation{Key: key, Lo: lo, Hi: hi, Op: OpRemoveRange}
}

// DeleteKey drops every ordinal under a key.
func DeleteKey(key uint64) Mutation {
	return Mutation{Key: key, Op: OpDeleteKey}
}

// Validate checks everything the server checks, so a malformed mutation fails
// here rather than after a round trip and a partially staged transaction.
func (m Mutation) Validate() error {
	switch m.Op {
	case OpDeleteKey:
		// A whole-key delete names no range, and the server refuses one that
		// carries bounds rather than ignoring them.
		if m.Lo != 0 || m.Hi != 0 {
			return fmt.Errorf("delete of key %d carries bounds %d..=%d; it names no range", m.Key, m.Lo, m.Hi)
		}
		return nil
	case OpInsert, OpRemove:
		// A point operation is a range whose bounds are equal. Sending one
		// with hi != lo is a transposed argument, not a range.
		if m.Lo != m.Hi {
			return fmt.Errorf("point operation on key %d has lo %d and hi %d; set hi == lo, or use the range operation", m.Key, m.Lo, m.Hi)
		}
	case OpInsertRange, OpRemoveRange:
	default:
		return fmt.Errorf("unknown mutation op %d", m.Op)
	}
	if m.Lo == math.MaxUint64 || m.Hi == math.MaxUint64 {
		return fmt.Errorf("ordinal %d is reserved and cannot be stored", uint64(math.MaxUint64))
	}
	if m.Lo > m.Hi {
		return fmt.Errorf("range lower bound %d is above upper bound %d", m.Lo, m.Hi)
	}
	return nil
}

// MutationsSchema returns the mixed-operation DoPut schema.
func MutationsSchema() *arrow.Schema {
	return arrow.NewSchema([]arrow.Field{
		{Name: "key", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "lo", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "hi", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "op", Type: arrow.PrimitiveTypes.Uint8, Nullable: false},
	}, nil)
}

// OrdinalSchema returns the non-null UInt64 query-result schema.
func OrdinalSchema() *arrow.Schema {
	return arrow.NewSchema([]arrow.Field{{Name: "ordinal", Type: arrow.PrimitiveTypes.Uint64, Nullable: false}}, nil)
}

// PairsSchema returns the non-null UInt64 bulk-mutation schema.
func PairsSchema() *arrow.Schema {
	return arrow.NewSchema([]arrow.Field{
		{Name: "key", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "ordinal", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
	}, nil)
}

func validateOrdinalRecord(record arrow.RecordBatch) (*array.Uint64, error) {
	if record == nil || record.NumCols() != 1 {
		columns := int64(0)
		if record != nil {
			columns = record.NumCols()
		}
		return nil, protocolErrorf("query", "server returned %d columns instead of 1", columns)
	}
	field := record.Schema().Field(0)
	values, ok := record.Column(0).(*array.Uint64)
	if field.Name != "ordinal" || field.Nullable || !ok {
		return nil, protocolError("query", "server returned no non-null UInt64 `ordinal` column")
	}
	if values.NullN() != 0 {
		return nil, protocolError("query", "server returned null ordinals")
	}
	return values, nil
}

func validatePairsRecord(record arrow.RecordBatch) error {
	if record == nil || record.NumCols() != 2 {
		return errorsForPairRecord("record must contain exactly two columns")
	}
	fields := record.Schema().Fields()
	keys, keysOK := record.Column(0).(*array.Uint64)
	ordinals, ordinalsOK := record.Column(1).(*array.Uint64)
	if fields[0].Name != "key" || fields[1].Name != "ordinal" || fields[0].Nullable || fields[1].Nullable || !keysOK || !ordinalsOK {
		return errorsForPairRecord("record must contain non-null UInt64 `key` and `ordinal` columns")
	}
	if keys.NullN() != 0 || ordinals.NullN() != 0 {
		return errorsForPairRecord("record must not contain nulls")
	}
	for index := 0; index < ordinals.Len(); index++ {
		if ordinals.Value(index) == math.MaxUint64 {
			return errorsForPairRecord("math.MaxUint64 is reserved and cannot be stored as an ordinal")
		}
	}
	return nil
}

func errorsForPairRecord(problem string) error {
	return fmt.Errorf("invalid yesnodb pair record: %s", problem)
}
