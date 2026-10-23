package yesnodb

import (
	"encoding/binary"
	"fmt"
	"math"
	"math/rand"
	"slices"
	"strings"
	"testing"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
	"github.com/apache/arrow-go/v18/arrow/memory"
)

// decodeChunk reads back the documented layouts.
//
// An independent implementation on purpose: this is the oracle for the
// encoder's unit tests, and an oracle sharing code with the thing it checks
// would agree with it about a shared mistake. The integration test uses the
// real server instead, which is the stronger check; this one runs without a
// daemon and localizes a failure to the byte layout.
func decodeChunk(t *testing.T, kind uint8, cardinality uint32, payload []byte) []uint16 {
	t.Helper()
	switch kind {
	case KindArray:
		if len(payload) != 2*int(cardinality) {
			t.Fatalf("array payload is %d bytes for cardinality %d", len(payload), cardinality)
		}
		values := make([]uint16, 0, cardinality)
		for index := 0; index < len(payload); index += 2 {
			values = append(values, binary.LittleEndian.Uint16(payload[index:]))
		}
		return values
	case KindBitmap:
		if len(payload) != BitmapBytes {
			t.Fatalf("bitmap payload is %d bytes", len(payload))
		}
		values := make([]uint16, 0, cardinality)
		for value := 0; value < ChunkCard; value++ {
			if payload[value>>3]>>(value&7)&1 == 1 {
				values = append(values, uint16(value))
			}
		}
		return values
	case KindRun:
		count := int(binary.LittleEndian.Uint16(payload))
		if len(payload) != 2+4*count {
			t.Fatalf("run payload is %d bytes for %d runs", len(payload), count)
		}
		values := make([]uint16, 0, cardinality)
		for index := 0; index < count; index++ {
			start := binary.LittleEndian.Uint16(payload[2+4*index:])
			lengthMinusOne := binary.LittleEndian.Uint16(payload[4+4*index:])
			for offset := 0; offset <= int(lengthMinusOne); offset++ {
				values = append(values, start+uint16(offset))
			}
		}
		return values
	default:
		t.Fatalf("unknown kind %d", kind)
		return nil
	}
}

func ascending(start, end, step int) []uint16 {
	values := make([]uint16, 0, (end-start+step-1)/step)
	for value := start; value < end; value += step {
		values = append(values, uint16(value))
	}
	return values
}

func TestAShortRunOfValuesIsAnArray(t *testing.T) {
	chunk, err := EncodeChunk([]uint16{1, 3, 5, 7})
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Kind != KindArray || chunk.Cardinality != 4 {
		t.Fatalf("chunk = %+v", chunk)
	}
	want := []byte{1, 0, 3, 0, 5, 0, 7, 0}
	if string(chunk.Payload) != string(want) {
		t.Fatalf("payload = %v, want little-endian %v", chunk.Payload, want)
	}
}

func TestAContiguousSpanIsOneRun(t *testing.T) {
	chunk, err := EncodeChunk(ascending(100, 5000, 1))
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Kind != KindRun || chunk.Cardinality != 4900 {
		t.Fatalf("chunk = %+v", chunk)
	}
	// One run count, then (start, length - 1).
	want := []byte{1, 0, 100, 0, 0x23, 0x13}
	if string(chunk.Payload) != string(want) {
		t.Fatalf("payload = %v, want %v", chunk.Payload, want)
	}
	if got := binary.LittleEndian.Uint16(chunk.Payload[4:]); got != 4899 {
		t.Fatalf("length_minus_one = %d, want 4899", got)
	}
}

func TestADenseScatterIsABitmap(t *testing.T) {
	// Every other value: 32,768 values is 65,536 bytes as an array and 131,074
	// as runs, so only the fixed 8 KiB bitmap is viable.
	values := ascending(0, ChunkCard, 2)
	chunk, err := EncodeChunk(values)
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Kind != KindBitmap || int(chunk.Cardinality) != len(values) || len(chunk.Payload) != BitmapBytes {
		t.Fatalf("chunk kind %d cardinality %d bytes %d", chunk.Kind, chunk.Cardinality, len(chunk.Payload))
	}
	if got := decodeChunk(t, chunk.Kind, chunk.Cardinality, chunk.Payload); !slices.Equal(got, values) {
		t.Fatalf("bitmap decoded %d values, want %d", len(got), len(values))
	}
}

func TestTheEncodingIsWhicheverIsSmallest(t *testing.T) {
	// 4,096 values, no two adjacent: an array at the cardinality ceiling is
	// 8,192 bytes, exactly the bitmap, and runs would be 16,386. The tie goes
	// to the array because KindArray is the lower discriminant, which is
	// arbitrary but must be stable.
	values := ascending(0, 2*ArrayMax, 2)
	chunk, err := EncodeChunk(values)
	if err != nil {
		t.Fatal(err)
	}
	if len(chunk.Payload) != BitmapBytes {
		t.Fatalf("payload is %d bytes, want the %d both encodings cost", len(chunk.Payload), BitmapBytes)
	}
	if chunk.Kind != KindArray {
		t.Fatalf("kind = %d, want a tie to resolve to KindArray", chunk.Kind)
	}

	// One more value and the array is gone on cardinality alone, while runs are
	// still far worse than the bitmap.
	chunk, err = EncodeChunk(slices.Insert(values, 1, uint16(1)))
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Cardinality != ArrayMax+1 || chunk.Kind != KindBitmap {
		t.Fatalf("chunk = kind %d cardinality %d", chunk.Kind, chunk.Cardinality)
	}
}

func TestAFullChunkIsOneRun(t *testing.T) {
	chunk, err := EncodeChunk(ascending(0, ChunkCard, 1))
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Kind != KindRun || chunk.Cardinality != ChunkCard {
		t.Fatalf("chunk = kind %d cardinality %d", chunk.Kind, chunk.Cardinality)
	}
	want := []byte{1, 0, 0, 0, 0xff, 0xff}
	if string(chunk.Payload) != string(want) {
		t.Fatalf("payload = %v, want %v", chunk.Payload, want)
	}
}

func TestAClusteredChunkIsAMultiIntervalRun(t *testing.T) {
	// 100 blocks of 50: 5,000 values shut the array out on cardinality alone,
	// and 100 intervals cost 402 bytes against the bitmap's 8,192.
	//
	// This is the only hand-written case that puts more than one interval on
	// the wire. runsOf emits its last interval from a different statement than
	// the ones inside its loop, so a sabotage of the loop's length arithmetic
	// alone is invisible to every single-interval case -- and it was invisible
	// to this whole suite until this test and the sweep's "clustered runs"
	// shape were added.
	values := make([]uint16, 0, 5000)
	for block := 0; block < 100; block++ {
		for offset := 0; offset < 50; offset++ {
			values = append(values, uint16(block*100+offset))
		}
	}
	chunk, err := EncodeChunk(values)
	if err != nil {
		t.Fatal(err)
	}
	if chunk.Kind != KindRun || chunk.Cardinality != 5000 {
		t.Fatalf("chunk = kind %d cardinality %d", chunk.Kind, chunk.Cardinality)
	}
	if count := binary.LittleEndian.Uint16(chunk.Payload); count != 100 {
		t.Fatalf("run count = %d, want 100", count)
	}
	if len(chunk.Payload) != 2+4*100 {
		t.Fatalf("run payload is %d bytes for 100 intervals", len(chunk.Payload))
	}
	// The second interval, which is the one written inside runsOf's loop.
	if start := binary.LittleEndian.Uint16(chunk.Payload[6:]); start != 100 {
		t.Fatalf("second interval starts at %d, want 100", start)
	}
	if length := binary.LittleEndian.Uint16(chunk.Payload[8:]); length != 49 {
		t.Fatalf("second interval has length_minus_one %d, want 49 for 50 values", length)
	}
	if got := decodeChunk(t, chunk.Kind, chunk.Cardinality, chunk.Payload); !slices.Equal(got, values) {
		t.Fatalf("a 100-interval run round-tripped %d values, want %d", len(got), len(values))
	}
}

func TestAdjacentRunsAreCoalesced(t *testing.T) {
	// The server's decoder repairs adjacency rather than refusing it, but only
	// because a foreign file may carry it. Emitting it would make the server
	// undo this encoder's work, so the intervals must already be maximal.
	chunk, err := EncodeChunk([]uint16{0, 1, 2, 3, 4, 5, 6, 7})
	if err != nil {
		t.Fatal(err)
	}
	want := []byte{1, 0, 0, 0, 7, 0}
	if string(chunk.Payload) != string(want) {
		t.Fatalf("payload = %v, want one maximal run %v", chunk.Payload, want)
	}
}

func TestAMalformedChunkIsRefusedBeforeTheWire(t *testing.T) {
	for _, test := range []struct {
		name    string
		values  []uint16
		message string
	}{
		{"empty", nil, "at least one value"},
		{"repeated", []uint16{5, 5}, "ascend and be unique"},
		{"descending", []uint16{5, 4}, "ascend and be unique"},
		{"unsorted tail", []uint16{1, 9, 4}, "ascend and be unique"},
	} {
		t.Run(test.name, func(t *testing.T) {
			_, err := EncodeChunk(test.values)
			if err == nil || !strings.Contains(err.Error(), test.message) {
				t.Fatalf("EncodeChunk = %v, want an error mentioning %q", err, test.message)
			}
		})
	}
}

// chunkShapes spans the three encodings on purpose, and spans the shapes
// *within* an encoding too.
//
// Which encoding wins is decided by cardinality and run count together, and a
// single generator cannot reach all three: a bitmap can only win above ArrayMax
// values, because an array at the ceiling costs exactly the bitmap's 8,192
// bytes, while runs win only when the values cluster. A uniform draw produces
// arrays and nothing else, and the first run-biased generator written here
// produced runs and nothing else -- which is what the coverage assertion below
// caught.
//
// "clustered runs" is here for a sharper reason. Every other shape that reaches
// a run encoding reaches a *single* interval, so a sabotage of the interval
// length that spared the last one went undetected by the whole suite. A
// multi-interval run container needs cardinality above ArrayMax to shut the
// array out and clusters long enough to beat the bitmap, and nothing produces
// that by accident.
var chunkShapes = []struct {
	name   string
	runLen int
	gapMax int
	size   int
}{
	{"contiguous", ChunkCard, 1, 3000},
	{"clustered runs", 50, 8, 6000},
	{"dense scatter", 1, 2, 20000},
	{"sparse", 1, 64, 400},
	{"very sparse", 1, 1024, 40},
	{"singleton", 1, 1, 1},
}

// randomChunk returns ascending unique values in runs of at most runLen
// separated by gaps of at least one and at most gapMax.
func randomChunk(random *rand.Rand, runLen, gapMax, size int) []uint16 {
	values := make([]uint16, 0, size)
	value := random.Intn(gapMax)
	for len(values) < size && value < ChunkCard {
		length := 1 + random.Intn(runLen)
		for offset := 0; offset < length && len(values) < size && value < ChunkCard; offset++ {
			values = append(values, uint16(value))
			value++
		}
		value += 1 + random.Intn(gapMax)
	}
	return values
}

func sweepChunks(t *testing.T, check func(t *testing.T, values []uint16, chunk Chunk)) {
	t.Helper()
	seen := map[uint8]int{}
	intervals := 0
	for _, shape := range chunkShapes {
		for seed := int64(0); seed < 30; seed++ {
			random := rand.New(rand.NewSource(seed))
			values := randomChunk(random, shape.runLen, shape.gapMax, shape.size)
			chunk, err := EncodeChunk(values)
			if err != nil {
				t.Fatalf("%s seed %d: %v", shape.name, seed, err)
			}
			t.Run(fmt.Sprintf("%s/%d", shape.name, seed), func(t *testing.T) {
				check(t, values, chunk)
			})
			seen[chunk.Kind]++
			if chunk.Kind == KindRun {
				intervals = max(intervals, int(binary.LittleEndian.Uint16(chunk.Payload)))
			}
		}
	}
	// Without these the sweep could pass having only ever built one encoding,
	// which is exactly what it did when first written, or having only ever
	// built single-interval runs, which is what let a sabotage through.
	for _, kind := range []uint8{KindArray, KindBitmap, KindRun} {
		if seen[kind] == 0 {
			t.Fatalf("the sweep never produced kind %d: %v", kind, seen)
		}
	}
	if intervals < 2 {
		t.Fatalf("the widest run container the sweep built had %d intervals", intervals)
	}
}

func TestAnyChunkRoundTrips(t *testing.T) {
	sweepChunks(t, func(t *testing.T, values []uint16, chunk Chunk) {
		if int(chunk.Cardinality) != len(values) {
			t.Fatalf("cardinality %d for %d values", chunk.Cardinality, len(values))
		}
		if got := decodeChunk(t, chunk.Kind, chunk.Cardinality, chunk.Payload); !slices.Equal(got, values) {
			t.Fatalf("kind %d round-tripped %d values, want %d", chunk.Kind, len(got), len(values))
		}
	})
}

func TestNoEncodingBeatsTheOneChosen(t *testing.T) {
	sweepChunks(t, func(t *testing.T, values []uint16, chunk Chunk) {
		runs := 1
		for index := 1; index < len(values); index++ {
			if values[index] != values[index-1]+1 {
				runs++
			}
		}
		smallest := min(BitmapBytes, 2+4*runs)
		if len(values) <= ArrayMax {
			smallest = min(smallest, 2*len(values))
		}
		if len(chunk.Payload) != smallest {
			t.Fatalf("kind %d cost %d bytes, want %d", chunk.Kind, len(chunk.Payload), smallest)
		}
	})
}

func TestASetIsSplitIntoChunksByItsHighBits(t *testing.T) {
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	if err := builder.PushSet(7, []uint64{1, 2, 1 << 16, (3 << 16) + 9}); err != nil {
		t.Fatal(err)
	}
	if builder.Len() != 3 || builder.Cardinality() != 4 {
		t.Fatalf("builder has %d rows and %d ordinals", builder.Len(), builder.Cardinality())
	}
	record := builder.Finish()
	if record == nil {
		t.Fatal("Finish returned nil for a builder holding three chunks")
	}
	defer record.Release()
	if !record.Schema().Equal(ContainersSchema()) {
		t.Fatalf("schema = %s", record.Schema())
	}
	keys := record.Column(0).(*array.Uint64)
	prefixes := record.Column(1).(*array.Uint64)
	cardinalities := record.Column(3).(*array.Uint32)
	payloads := record.Column(4).(*array.Binary)
	for index := 0; index < 3; index++ {
		if keys.Value(index) != 7 {
			t.Fatalf("row %d has key %d", index, keys.Value(index))
		}
	}
	if got := []uint64{prefixes.Value(0), prefixes.Value(1), prefixes.Value(2)}; !slices.Equal(got, []uint64{0, 1, 3}) {
		t.Fatalf("prefixes = %v, want the high 48 bits in ascending order", got)
	}
	if got := []uint32{cardinalities.Value(0), cardinalities.Value(1), cardinalities.Value(2)}; !slices.Equal(got, []uint32{2, 1, 1}) {
		t.Fatalf("cardinalities = %v", got)
	}
	kinds := record.Column(2).(*array.Uint8)
	if got := decodeChunk(t, kinds.Value(2), cardinalities.Value(2), payloads.Value(2)); !slices.Equal(got, []uint16{9}) {
		t.Fatalf("the third chunk holds %v, want the low bits of (3 << 16) + 9", got)
	}
}

func TestFinishingClearsTheBuilder(t *testing.T) {
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	if err := builder.PushSet(1, []uint64{1}); err != nil {
		t.Fatal(err)
	}
	record := builder.Finish()
	if record == nil {
		t.Fatal("Finish returned nil for a builder holding a chunk")
	}
	record.Release()
	if builder.Len() != 0 || builder.Cardinality() != 0 {
		t.Fatalf("builder kept %d rows and %d ordinals", builder.Len(), builder.Cardinality())
	}
	if again := builder.Finish(); again != nil {
		t.Fatalf("Finish on an empty builder = %v, want nil rather than a zero-row batch", again)
	}
}

func TestABuilderLeaksNothing(t *testing.T) {
	// Go's Arrow memory is released by hand, unlike the Python client's, so the
	// builder's own discipline is testable and worth testing.
	allocator := memory.NewCheckedAllocator(memory.DefaultAllocator)
	builder := NewContainerBatchBuilder(allocator)
	if err := builder.PushSet(1, []uint64{1, 2, 3, 1 << 20}); err != nil {
		t.Fatal(err)
	}
	record := builder.Finish()
	if record == nil {
		t.Fatal("Finish returned nil")
	}
	record.Release()
	builder.Release()
	allocator.AssertSize(t, 0)
}

func TestContainerBatchesLeakNothing(t *testing.T) {
	allocator := memory.NewCheckedAllocator(memory.DefaultAllocator)
	records, err := ContainerBatches(allocator, []Set{{Key: 1, Ordinals: []uint64{1, 2, 1 << 17}}}, 1)
	if err != nil {
		t.Fatal(err)
	}
	for _, record := range records {
		record.Release()
	}
	allocator.AssertSize(t, 0)
}

func TestUnsortedOrdinalsAndDuplicatesAreAcceptedForAWholeSet(t *testing.T) {
	// PushSet sorts and deduplicates, unlike PushChunk: a caller holding a
	// whole set has no order to offer, and a caller holding one chunk's values
	// is close enough to the wire that a silent sort would hide a bug.
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	if err := builder.PushSet(1, []uint64{9, 1, 9, 5}); err != nil {
		t.Fatal(err)
	}
	record := builder.Finish()
	if record == nil {
		t.Fatal("Finish returned nil")
	}
	defer record.Release()
	cardinalities := record.Column(3).(*array.Uint32)
	if cardinalities.Value(0) != 3 {
		t.Fatalf("cardinality = %d, want the duplicate dropped", cardinalities.Value(0))
	}
	kinds := record.Column(2).(*array.Uint8)
	payloads := record.Column(4).(*array.Binary)
	if got := decodeChunk(t, kinds.Value(0), cardinalities.Value(0), payloads.Value(0)); !slices.Equal(got, []uint16{1, 5, 9}) {
		t.Fatalf("chunk holds %v", got)
	}
}

func TestAReservedOrdinalIsRefused(t *testing.T) {
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	err := builder.PushSet(1, []uint64{math.MaxUint64})
	if err == nil || !strings.Contains(err.Error(), "reserved") {
		t.Fatalf("PushSet = %v, want a refusal naming the reserved ordinal", err)
	}
}

func TestAPrefixBeyond48BitsIsRefused(t *testing.T) {
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	err := builder.PushChunk(1, Prefix48Limit, []uint16{0})
	if err == nil || !strings.Contains(err.Error(), "48 bits") {
		t.Fatalf("PushChunk = %v, want a refusal naming the 48-bit bound", err)
	}
}

func TestBatchesAreBoundedByChunkCount(t *testing.T) {
	sets := make([]Set, 0, 10)
	for key := uint64(0); key < 10; key++ {
		sets = append(sets, Set{Key: key, Ordinals: []uint64{key << 16, (key + 1) << 16}})
	}
	records, err := ContainerBatches(nil, sets, 4)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		for _, record := range records {
			record.Release()
		}
	}()
	rows := make([]int64, 0, len(records))
	total := int64(0)
	for _, record := range records {
		rows = append(rows, record.NumRows())
		total += record.NumRows()
	}
	if !slices.Equal(rows, []int64{4, 4, 4, 4, 4}) || total != 20 {
		t.Fatalf("batch rows = %v totalling %d", rows, total)
	}

	// A key's chunks stay in one batch, so the bound is a floor once a key
	// straddles it rather than a ceiling.
	wide := make([]uint64, 0, 10)
	for index := uint64(0); index < 10; index++ {
		wide = append(wide, index<<16)
	}
	only, err := ContainerBatches(nil, []Set{{Key: 1, Ordinals: wide}}, 4)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		for _, record := range only {
			record.Release()
		}
	}()
	if len(only) != 1 || only[0].NumRows() != 10 {
		t.Fatalf("a ten-chunk key produced %d batches", len(only))
	}
}

func TestAnEmptyInputSendsNothing(t *testing.T) {
	records, err := ContainerBatches(nil, nil, containerBatchChunks)
	if err != nil || len(records) != 0 {
		t.Fatalf("ContainerBatches(nil) = %d batches, %v", len(records), err)
	}
	records, err = ContainerBatches(nil, []Set{{Key: 1}}, containerBatchChunks)
	if err != nil || len(records) != 0 {
		t.Fatalf("a set with no ordinals produced %d batches, %v", len(records), err)
	}
	if _, err := ContainerBatches(nil, nil, 0); err == nil {
		t.Fatal("maxChunks of zero must be refused")
	}
}

func TestTheContainerSchemaMatchesTheServersFieldForField(t *testing.T) {
	// The server dispatches on schema equality: a container batch that misses
	// by a name or an integer width is read as the mutation wire instead.
	schema := ContainersSchema()
	names := make([]string, 0, 5)
	for _, field := range schema.Fields() {
		names = append(names, field.Name)
		if field.Nullable {
			t.Fatalf("field %s is nullable", field.Name)
		}
	}
	if !slices.Equal(names, []string{"key", "prefix48", "kind", "cardinality", "payload"}) {
		t.Fatalf("fields = %v", names)
	}
	types := []arrow.DataType{
		arrow.PrimitiveTypes.Uint64,
		arrow.PrimitiveTypes.Uint64,
		arrow.PrimitiveTypes.Uint8,
		arrow.PrimitiveTypes.Uint32,
		arrow.BinaryTypes.Binary,
	}
	for index, want := range types {
		if !arrow.TypeEqual(schema.Field(index).Type, want) {
			t.Fatalf("field %s is %s, want %s", schema.Field(index).Name, schema.Field(index).Type, want)
		}
	}
}

func TestAWrongSchemaBatchNeverReachesTheServer(t *testing.T) {
	// uint32 rather than uint64 keys: close enough that only an exact
	// comparison catches it, and the server would read it as neither wire.
	wrong := arrow.NewSchema([]arrow.Field{
		{Name: "key", Type: arrow.PrimitiveTypes.Uint32, Nullable: false},
		{Name: "prefix48", Type: arrow.PrimitiveTypes.Uint64, Nullable: false},
		{Name: "kind", Type: arrow.PrimitiveTypes.Uint8, Nullable: false},
		{Name: "cardinality", Type: arrow.PrimitiveTypes.Uint32, Nullable: false},
		{Name: "payload", Type: arrow.BinaryTypes.Binary, Nullable: false},
	}, nil)
	builder := array.NewRecordBuilder(memory.DefaultAllocator, wrong)
	defer builder.Release()
	builder.Field(0).(*array.Uint32Builder).Append(1)
	builder.Field(1).(*array.Uint64Builder).Append(0)
	builder.Field(2).(*array.Uint8Builder).Append(KindArray)
	builder.Field(3).(*array.Uint32Builder).Append(1)
	builder.Field(4).(*array.BinaryBuilder).Append([]byte{7, 0})
	record := builder.NewRecordBatch()
	defer record.Release()

	if _, err := validateContainerRecord(record); err == nil {
		t.Fatal("a UInt32 key column must be refused before the wire")
	}
	if _, err := validateContainerRecord(nil); err == nil {
		t.Fatal("a nil record must be refused")
	}
}

func TestTheValidatorReportsCardinalityRatherThanRows(t *testing.T) {
	// The acknowledgement comparison is a checksum of the server's decode, so
	// the number the client sends for comparison has to be the ordinal count.
	// One row here carries 5,000 of them.
	builder := NewContainerBatchBuilder(nil)
	defer builder.Release()
	ordinals := make([]uint64, 0, 5000)
	for ordinal := uint64(0); ordinal < 5000; ordinal++ {
		ordinals = append(ordinals, ordinal)
	}
	if err := builder.PushSet(3, ordinals); err != nil {
		t.Fatal(err)
	}
	record := builder.Finish()
	if record == nil {
		t.Fatal("Finish returned nil")
	}
	defer record.Release()
	cardinality, err := validateContainerRecord(record)
	if err != nil {
		t.Fatal(err)
	}
	if record.NumRows() != 1 {
		t.Fatalf("5,000 contiguous ordinals took %d rows", record.NumRows())
	}
	if cardinality != 5000 {
		t.Fatalf("validator reported %d, want the 5,000 ordinals rather than the 1 row", cardinality)
	}
}
