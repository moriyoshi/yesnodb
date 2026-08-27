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
