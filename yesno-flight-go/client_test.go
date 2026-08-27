package yesnodb

import (
	"context"
	"errors"
	"io"
	"reflect"
	"testing"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
	"github.com/apache/arrow-go/v18/arrow/flight"
	"github.com/apache/arrow-go/v18/arrow/ipc"
	"github.com/apache/arrow-go/v18/arrow/memory"
	"google.golang.org/grpc/metadata"
	"google.golang.org/protobuf/proto"
)

func queryTicketBytes(t *testing.T) []byte {
	t.Helper()
	wire, err := (Ticket{Version: 9, Key: 42, PrefixHi: 1 << 48}).Encode()
	if err != nil {
		t.Fatal(err)
	}
	return wire
}

func flightInfoForTest(t *testing.T, schema *arrow.Schema, count int64, tickets ...[]byte) *flight.FlightInfo {
	t.Helper()
	endpoints := make([]*flight.FlightEndpoint, len(tickets))
	for index, ticket := range tickets {
		endpoints[index] = &flight.FlightEndpoint{Ticket: &flight.Ticket{Ticket: append([]byte(nil), ticket...)}}
	}
	return &flight.FlightInfo{
		Schema:       flight.SerializeSchema(schema, memory.DefaultAllocator),
		Endpoint:     endpoints,
		TotalRecords: count,
	}
}

func TestQueryInfoRejectsMalformedFlightMetadata(t *testing.T) {
	t.Parallel()
	client := newConfiguredClient(nil, defaultClientConfig())
	ticket := queryTicketBytes(t)
	valid := flightInfoForTest(t, OrdinalSchema(), 3, ticket)
	if info, err := client.queryInfo(valid); err != nil || info.TotalRecords() != 3 || info.Version() != 9 {
		t.Fatalf("valid QueryInfo = %#v, %v", info, err)
	}

	cases := []struct {
		name string
		info *flight.FlightInfo
	}{
		{"negative count", flightInfoForTest(t, OrdinalSchema(), -1, ticket)},
		{"no endpoint", flightInfoForTest(t, OrdinalSchema(), 0)},
		{"two endpoints", flightInfoForTest(t, OrdinalSchema(), 0, ticket, ticket)},
		{"malformed ticket", flightInfoForTest(t, OrdinalSchema(), 0, []byte{1, 2, 3})},
		{"wrong schema", flightInfoForTest(t, PairsSchema(), 0, ticket)},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			if _, err := client.queryInfo(test.info); err == nil {
				t.Fatal("malformed FlightInfo was accepted")
			} else {
				var protocol *ProtocolError
				if !errors.As(err, &protocol) {
					t.Fatalf("error %T is not ProtocolError: %v", err, err)
				}
			}
		})
	}
}

type flightDataSink struct{ messages []*flight.FlightData }

func (s *flightDataSink) Send(message *flight.FlightData) error {
	s.messages = append(s.messages, proto.Clone(message).(*flight.FlightData))
	return nil
}

type flightDataSource struct {
	messages []*flight.FlightData
	index    int
}

func (s *flightDataSource) Recv() (*flight.FlightData, error) {
	if s.index == len(s.messages) {
		return nil, io.EOF
	}
	message := s.messages[s.index]
	s.index++
	return message, nil
}

func uint64Record(t *testing.T, schema *arrow.Schema, values []uint64, nullAt int) arrow.RecordBatch {
	t.Helper()
	builder := array.NewUint64Builder(memory.DefaultAllocator)
	defer builder.Release()
	for index, value := range values {
		if index == nullAt {
			builder.AppendNull()
		} else {
			builder.Append(value)
		}
	}
	column := builder.NewArray()
	defer column.Release()
	return array.NewRecordBatch(schema, []arrow.Array{column}, int64(len(values)))
}

func queryStreamForRecords(t *testing.T, promised uint64, schema *arrow.Schema, records ...arrow.RecordBatch) *QueryStream {
	t.Helper()
	sink := &flightDataSink{}
	writer := flight.NewRecordWriter(sink, ipc.WithSchema(schema))
	for _, record := range records {
		if err := writer.Write(record); err != nil {
			t.Fatal(err)
		}
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	reader, err := flight.NewRecordReader(&flightDataSource{messages: sink.messages})
	if err != nil {
		t.Fatal(err)
	}
	return &QueryStream{info: &QueryInfo{totalRecords: promised}, reader: reader}
}

func TestQueryStreamValidatesOrderCountSchemaAndNullability(t *testing.T) {
	t.Parallel()
	t.Run("valid across batches", func(t *testing.T) {
		first := uint64Record(t, OrdinalSchema(), []uint64{1, 3}, -1)
		second := uint64Record(t, OrdinalSchema(), []uint64{5, 8}, -1)
		defer first.Release()
		defer second.Release()
		stream := queryStreamForRecords(t, 4, OrdinalSchema(), first, second)
		defer stream.Release()
		got, err := stream.CollectOrdinals()
		if err != nil || !reflect.DeepEqual(got, []uint64{1, 3, 5, 8}) {
			t.Fatalf("ordinals = %v, %v", got, err)
		}
	})
	t.Run("cross-batch disorder", func(t *testing.T) {
		first := uint64Record(t, OrdinalSchema(), []uint64{1, 5}, -1)
		second := uint64Record(t, OrdinalSchema(), []uint64{5, 8}, -1)
		defer first.Release()
		defer second.Release()
		stream := queryStreamForRecords(t, 4, OrdinalSchema(), first, second)
		defer stream.Release()
		if _, err := stream.CollectOrdinals(); err == nil {
			t.Fatal("duplicate across batch boundary was accepted")
		}
	})
	t.Run("short count", func(t *testing.T) {
		record := uint64Record(t, OrdinalSchema(), []uint64{1, 3}, -1)
		defer record.Release()
		stream := queryStreamForRecords(t, 3, OrdinalSchema(), record)
		defer stream.Release()
		if _, err := stream.CollectOrdinals(); err == nil {
			t.Fatal("short result was accepted")
		}
	})
	t.Run("nullable field", func(t *testing.T) {
		schema := arrow.NewSchema([]arrow.Field{{Name: "ordinal", Type: arrow.PrimitiveTypes.Uint64, Nullable: true}}, nil)
		record := uint64Record(t, schema, []uint64{1}, -1)
		defer record.Release()
		stream := queryStreamForRecords(t, 1, schema, record)
		defer stream.Release()
		if _, err := stream.CollectOrdinals(); err == nil {
			t.Fatal("nullable ordinal field was accepted")
		}
	})
	t.Run("null value", func(t *testing.T) {
		record := uint64Record(t, OrdinalSchema(), []uint64{1}, 0)
		defer record.Release()
		stream := queryStreamForRecords(t, 1, OrdinalSchema(), record)
		defer stream.Release()
		if _, err := stream.CollectOrdinals(); err == nil {
			t.Fatal("null ordinal was accepted")
		}
	})
}

func TestRPCMetadataCarriesCredentialsAndMonotonicTerm(t *testing.T) {
	t.Parallel()
	config := defaultClientConfig()
	if err := WithBearerToken("secret")(&config); err != nil {
		t.Fatal(err)
	}
	if err := WithMinimumTerm(4)(&config); err != nil {
		t.Fatal(err)
	}
	client := newConfiguredClient(nil, config)
	ctx, err := client.rpcContext(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	metadata, ok := metadata.FromOutgoingContext(ctx)
	if !ok || !reflect.DeepEqual(metadata.Get("authorization"), []string{"Bearer secret"}) || !reflect.DeepEqual(metadata.Get(headerExpectTerm), []string{"4"}) {
		t.Fatalf("outgoing metadata = %v", metadata)
	}
	if err := client.observeHeaders(metadata2(headerTerm, "9")); err != nil {
		t.Fatal(err)
	}
	if err := client.observeHeaders(metadata2(headerTerm, "7")); err != nil {
		t.Fatal(err)
	}
	if client.MinimumTerm() != 9 {
		t.Fatalf("minimum term regressed to %d", client.MinimumTerm())
	}
	if err := client.observeHeaders(metadata2(headerTerm, "not-a-number")); err == nil {
		t.Fatal("malformed response term was accepted")
	}
}

func metadata2(key, value string) metadata.MD {
	return metadata.Pairs(key, value)
}

func TestDialRequiresExplicitTransportSecurity(t *testing.T) {
	t.Parallel()
	if _, err := Dial("127.0.0.1:1"); err == nil {
		t.Fatal("Dial accepted an implicit transport mode")
	}
}
