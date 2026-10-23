package yesnodb

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"slices"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
	"github.com/apache/arrow-go/v18/arrow/memory"
)

// Container payloads for the write wire.
//
// The pair and mutation wires cost one row per set bit. A container row carries
// one chunk of up to 65,536 ordinals as the Roaring-spec bytes the page store
// already holds, which the server measured at 64.8x less wire on a half-dense
// page.
//
// This is an encoder and nothing more. There is no Roaring dependency and none
// is wanted: a container payload is three fixed layouts over sorted 16-bit
// values, all three documented in the storage format, and this file implements
// exactly what that table says. The server validates every payload through a
// decoder that is a fuzz target by contract, so a payload built wrong here
// comes back as InvalidArgument rather than as corruption.
//
// Insert only. A container row unions into whatever is already under the key:
// the write wire has no op column, because giving it one would fork it from the
// read side's schema, which is the single property that makes the whole thing
// cheap. A removal is RemoveRange on the mutation wire.

const (
	// ChunkBits is how many of an ordinal's low bits index within one
	// container. The remaining 48 are the chunk's prefix.
	ChunkBits = 16
	// ChunkCard is how many ordinals one container addresses.
	ChunkCard = 1 << ChunkBits
	// ArrayMax is the highest cardinality an array payload may carry, whatever
	// its byte count says. The server's decoder refuses a longer one.
	ArrayMax = 4096
	// BitmapBytes is a bitmap payload's length, whatever its cardinality.
	BitmapBytes = 8192
	// Prefix48Limit bounds a chunk's high 48 bits.
	Prefix48Limit = 1 << 48

	// containerBatchChunks is the chunks per record batch, so one batch stays a
	// bounded allocation. A bitmap chunk is 8 KiB of payload, so this is a
	// ~16 MiB ceiling on the values buffer.
	containerBatchChunks = 2048
)

// Payload layouts, as stored. These numbers go on the wire.
const (
	KindArray  uint8 = 0
	KindBitmap uint8 = 1
	KindRun    uint8 = 2
)

// Capability bit FeatureContainerPut in schema.go reports whether the server
// accepts these batches. Ask Supports before sending one: a server that
// predates the wire rejects the batch, because apply accepts only schemas it
// knows.

// ContainersSchema returns the container-payload DoPut schema.
//
// Byte-identical to what the page store holds and to what the read side ships,
// which is why the write half needed no new schema. The server picks the wire
// by comparing schemas, so a batch that misses by a field name or an integer
// width is read as the mutation schema instead.
//
// kind and cardinality are columns rather than comments because neither is
// derivable from payload alone: an array of n values and a run of n/2 intervals
// can be the same length, and a bitmap's cardinality is a popcount the reader
// should not have to redo.
func ContainersSchema() *arrow.Schema {
	return arrow.NewSchema([]arrow.Field{
		{Name: "key", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "prefix48", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "kind", Type: arrow.PrimitiveTypes.Uint8, Nullable: false},
		{Name: "cardinality", Type: arrow.PrimitiveTypes.Uint32, Nullable: false},
		{Name: "payload", Type: arrow.BinaryTypes.Binary, Nullable: false},
	}, nil)
}

// Chunk is one encoded container.
type Chunk struct {
	// Kind is the payload layout: KindArray, KindBitmap or KindRun.
	Kind uint8
	// Cardinality is how many ordinals the payload holds, which is what the
	// server acknowledges for a container write.
	Cardinality uint32
	// Payload is the container bytes, byte-identical to what the page store
	// holds and what a portable Roaring file carries.
	Payload []byte
}

// Set is one key and the ordinals to union into it.
type Set struct {
	Key      uint64
	Ordinals []uint64
}

type runInterval struct {
	start          uint16
	lengthMinusOne uint16
}

// runsOf returns maximal ascending intervals over values.
//
// Maximal, not merely non-overlapping. The Roaring spec permits adjacent runs
// and the server's decoder coalesces them, but it does so only because a
// foreign file may carry them; it is never legal state inside the engine, whose
// run kernels assume maximal intervals. Emitting them would make the server do
// work to undo this function's output.
func runsOf(values []uint16) []runInterval {
	runs := make([]runInterval, 0, 8)
	start, previous := values[0], values[0]
	for _, value := range values[1:] {
		// previous+1 cannot wrap: 65,535 can only be the last of a strictly
		// ascending slice, so the loop has already ended by then.
		if value == previous+1 {
			previous = value
			continue
		}
		runs = append(runs, runInterval{start, previous - start})
		start, previous = value, value
	}
	return append(runs, runInterval{start, previous - start})
}

func arrayPayload(values []uint16) []byte {
	payload := make([]byte, 2*len(values))
	for index, value := range values {
		binary.LittleEndian.PutUint16(payload[2*index:], value)
	}
	return payload
}

func bitmapPayload(values []uint16) []byte {
	// 1024 little-endian u64 words, bit i representing value i. Within a
	// little-endian word, byte b holds bits 8b..8b+8, so the whole payload is
	// one flat LSB-first bit array and the word size never appears here.
	payload := make([]byte, BitmapBytes)
	for _, value := range values {
		payload[value>>3] |= 1 << (value & 7)
	}
	return payload
}

func runPayload(runs []runInterval) []byte {
	payload := make([]byte, 2+4*len(runs))
	binary.LittleEndian.PutUint16(payload, uint16(len(runs)))
	for index, run := range runs {
		binary.LittleEndian.PutUint16(payload[2+4*index:], run.start)
		binary.LittleEndian.PutUint16(payload[4+4*index:], run.lengthMinusOne)
	}
	return payload
}

// EncodeChunk returns the smallest valid container for one chunk's values.
//
// values are the low ChunkBits of one chunk's ordinals, ascending and unique.
// Both are checked: the server's decoder would catch a descending array
// payload, but it cannot catch a caller who passed unsorted values and got a
// run encoding, because the runs derived from them are structurally valid and
// describe a different set.
//
// Typing the values as uint16 is doing work here. The range check and the
// 65,536 length ceiling that an untyped encoder has to assert are both
// unrepresentable, and so is a run count above the decoder's 32,768-interval
// bound, which strictly ascending uint16 values cannot reach.
//
// Smallest, rather than the engine's own choice. yesno-core picks a
// representation with hysteresis -- a bitmap demotes only below a cardinality
// well under the one at which an array would first be smaller -- because it is
// deciding whether to re-encode a container that is being mutated, and flapping
// between two representations costs more than either. A fresh encode has
// nothing to flap against, so the only question is bytes on the wire, and the
// engine normalizes what it receives anyway.
func EncodeChunk(values []uint16) (Chunk, error) {
	if len(values) == 0 {
		return Chunk{}, errors.New("a container needs at least one value")
	}
	for index := 1; index < len(values); index++ {
		if values[index] <= values[index-1] {
			return Chunk{}, fmt.Errorf(
				"chunk values must ascend and be unique: value %d at index %d follows %d",
				values[index], index, values[index-1])
		}
	}

	runs := runsOf(values)

	// Every viable candidate, then the smallest. The list is in kind order and
	// a candidate replaces the incumbent only on a strictly smaller size, so a
	// tie goes to the lowest kind -- arbitrary, but it has to be stable.
	// array drops out above ArrayMax whatever its byte count says.
	type candidate struct {
		size int
		kind uint8
	}
	candidates := make([]candidate, 0, 3)
	if len(values) <= ArrayMax {
		candidates = append(candidates, candidate{2 * len(values), KindArray})
	}
	candidates = append(candidates, candidate{BitmapBytes, KindBitmap})
	candidates = append(candidates, candidate{2 + 4*len(runs), KindRun})
	chosen := candidates[0]
	for _, next := range candidates[1:] {
		if next.size < chosen.size {
			chosen = next
		}
	}

	var payload []byte
	switch chosen.kind {
	case KindArray:
		payload = arrayPayload(values)
	case KindBitmap:
		payload = bitmapPayload(values)
	default:
		payload = runPayload(runs)
	}
	// Not tautological: each payload builder owns its own length expression,
	// and this is what catches one of them drifting from the size it was
	// chosen by.
	if len(payload) != chosen.size {
		return Chunk{}, fmt.Errorf(
			"kind %d encoded %d bytes but was chosen at %d",
			chosen.kind, len(payload), chosen.size)
	}
	return Chunk{Kind: chosen.kind, Cardinality: uint32(len(values)), Payload: payload}, nil
}

// ContainerBatchBuilder accumulates chunks and hands them over as record
// batches.
//
// Finish returns nil for an empty builder rather than an empty batch, because a
// DoPut stream carrying a zero-row batch is a different thing from one carrying
// nothing.
//
// A builder owns Arrow memory until it is released. Release is safe to call
// after Finish and safe to defer; the batches Finish returns are the caller's
// and outlive it.
type ContainerBatchBuilder struct {
	builder     *array.RecordBuilder
	rows        int
	cardinality uint64
}

// NewContainerBatchBuilder returns an empty builder. A nil allocator means
// memory.DefaultAllocator.
func NewContainerBatchBuilder(allocator memory.Allocator) *ContainerBatchBuilder {
	if allocator == nil {
		allocator = memory.DefaultAllocator
	}
	return &ContainerBatchBuilder{builder: array.NewRecordBuilder(allocator, ContainersSchema())}
}

// Len reports the chunks staged so far.
func (b *ContainerBatchBuilder) Len() int { return b.rows }

// Cardinality reports the ordinals staged so far, which is what the server
// acknowledges.
func (b *ContainerBatchBuilder) Cardinality() uint64 { return b.cardinality }

// PushChunk appends one chunk, encoded by EncodeChunk.
func (b *ContainerBatchBuilder) PushChunk(key, prefix48 uint64, values []uint16) error {
	if prefix48 >= Prefix48Limit {
		return fmt.Errorf("prefix48 %d does not fit 48 bits", prefix48)
	}
	chunk, err := EncodeChunk(values)
	if err != nil {
		return err
	}
	b.builder.Field(0).(*array.Uint64Builder).Append(key)
	b.builder.Field(1).(*array.Uint64Builder).Append(prefix48)
	b.builder.Field(2).(*array.Uint8Builder).Append(chunk.Kind)
	b.builder.Field(3).(*array.Uint32Builder).Append(chunk.Cardinality)
	b.builder.Field(4).(*array.BinaryBuilder).Append(chunk.Payload)
	b.rows++
	b.cardinality += uint64(chunk.Cardinality)
	return nil
}

// PushSet appends every chunk of one key's ordinals, which need not arrive
// sorted and may repeat.
//
// Unlike PushChunk, which refuses unsorted values: a caller holding a whole set
// has no order to offer, and a caller holding one chunk's values is close enough
// to the wire that a silent sort would hide a bug.
func (b *ContainerBatchBuilder) PushSet(key uint64, ordinals []uint64) error {
	chunks := make(map[uint64][]uint16)
	for _, ordinal := range ordinals {
		if ordinal == math.MaxUint64 {
			return fmt.Errorf("ordinal %d is reserved and cannot be stored", ordinal)
		}
		// The conversion truncates to the low ChunkBits, which is the mask.
		chunks[ordinal>>ChunkBits] = append(chunks[ordinal>>ChunkBits], uint16(ordinal))
	}
	prefixes := make([]uint64, 0, len(chunks))
	for prefix := range chunks {
		prefixes = append(prefixes, prefix)
	}
	slices.Sort(prefixes)
	for _, prefix := range prefixes {
		values := chunks[prefix]
		slices.Sort(values)
		if err := b.PushChunk(key, prefix, slices.Compact(values)); err != nil {
			return err
		}
	}
	return nil
}

// Finish returns the accumulated chunks and resets the builder, or nil when
// nothing was staged. The caller owns the batch and must release it.
func (b *ContainerBatchBuilder) Finish() arrow.RecordBatch {
	if b.rows == 0 {
		return nil
	}
	b.rows = 0
	b.cardinality = 0
	return b.builder.NewRecordBatch()
}

// Release frees the builder's Arrow memory.
func (b *ContainerBatchBuilder) Release() { b.builder.Release() }

// ContainerBatches encodes sets into batches of at most maxChunks chunks. A nil
// allocator means memory.DefaultAllocator.
//
// A key's chunks are not split across batches, so a batch may exceed maxChunks
// by the chunk count of its last key. That keeps one key's write in one batch,
// which is the property a caller is more likely to depend on than an exact
// batch size.
//
// The caller owns the returned batches and must release each one.
func ContainerBatches(allocator memory.Allocator, sets []Set, maxChunks int) ([]arrow.RecordBatch, error) {
	if maxChunks < 1 {
		return nil, fmt.Errorf("maxChunks must be at least 1, got %d", maxChunks)
	}
	builder := NewContainerBatchBuilder(allocator)
	defer builder.Release()
	var records []arrow.RecordBatch
	for _, set := range sets {
		if err := builder.PushSet(set.Key, set.Ordinals); err != nil {
			for _, record := range records {
				record.Release()
			}
			return nil, err
		}
		if builder.Len() >= maxChunks {
			if record := builder.Finish(); record != nil {
				records = append(records, record)
			}
		}
	}
	if record := builder.Finish(); record != nil {
		records = append(records, record)
	}
	return records, nil
}

// validateContainerRecord checks a caller-built batch and returns its
// cardinality.
//
// Cardinality, not rows. The server acknowledges the ordinals it applied, and a
// container row carries up to 65,536 of them, so returning it from the
// validator is what keeps a caller from counting the wrong thing.
func validateContainerRecord(record arrow.RecordBatch) (uint64, error) {
	if record == nil {
		return 0, errorsForContainerRecord("record must not be nil")
	}
	// Exact, because the server picks the wire by the schema: a batch that
	// misses by a field name or an integer width is read as the mutation
	// schema, which it also does not match, and refused as that instead.
	if !record.Schema().Equal(ContainersSchema()) {
		return 0, errorsForContainerRecord(fmt.Sprintf(
			"record schema must be %s, got %s", ContainersSchema(), record.Schema()))
	}
	for column := int64(0); column < record.NumCols(); column++ {
		if record.Column(int(column)).NullN() != 0 {
			return 0, errorsForContainerRecord("record must not contain nulls")
		}
	}
	cardinalities, ok := record.Column(3).(*array.Uint32)
	if !ok {
		return 0, errorsForContainerRecord("`cardinality` must be a UInt32 column")
	}
	var total uint64
	for index := 0; index < cardinalities.Len(); index++ {
		total += uint64(cardinalities.Value(index))
	}
	return total, nil
}

func errorsForContainerRecord(problem string) error {
	return fmt.Errorf("invalid yesnodb container record: %s", problem)
}
