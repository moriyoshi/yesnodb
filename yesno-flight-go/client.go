package yesnodb

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"math"
	"strconv"
	"strings"
	"sync/atomic"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
	"github.com/apache/arrow-go/v18/arrow/flight"
	"github.com/apache/arrow-go/v18/arrow/ipc"
	"github.com/apache/arrow-go/v18/arrow/memory"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/protobuf/proto"
)

const (
	batchRows        = 8192
	putInsert        = "insert"
	putRemove        = "remove"
	headerExpectTerm = "yesno-expect-term"
	headerTerm       = "yesno-term"
)

// TokenProvider returns the bearer token to send on one RPC.
// Implementations may rotate tokens and must be safe for concurrent use when
// the Client is used concurrently.
type TokenProvider func(context.Context) (string, error)

type clientConfig struct {
	allocator   memory.Allocator
	token       TokenProvider
	minimumTerm uint32
	credentials credentials.TransportCredentials
	grpcOptions []grpc.DialOption
}

func defaultClientConfig() clientConfig {
	return clientConfig{allocator: memory.DefaultAllocator}
}

// Option configures a Client or Dial operation.
type Option func(*clientConfig) error

// WithAllocator selects the allocator used for decoded and constructed Arrow values.
func WithAllocator(allocator memory.Allocator) Option {
	return func(config *clientConfig) error {
		if allocator == nil {
			return errors.New("yesnodb allocator must not be nil")
		}
		config.allocator = allocator
		return nil
	}
}

// WithBearerToken sends one fixed bearer token on every RPC.
func WithBearerToken(token string) Option {
	return WithTokenProvider(func(context.Context) (string, error) { return token, nil })
}

// WithTokenProvider obtains a bearer token separately for every RPC.
func WithTokenProvider(provider TokenProvider) Option {
	return func(config *clientConfig) error {
		if provider == nil {
			return errors.New("yesnodb token provider must not be nil")
		}
		config.token = provider
		return nil
	}
}

// WithMinimumTerm sets the lowest leadership term this client will accept.
// The client raises this floor monotonically when servers advertise newer terms.
func WithMinimumTerm(term uint32) Option {
	return func(config *clientConfig) error {
		config.minimumTerm = term
		return nil
	}
}

// WithInsecureTransport explicitly selects plaintext gRPC.
func WithInsecureTransport() Option {
	return func(config *clientConfig) error {
		config.credentials = insecure.NewCredentials()
		return nil
	}
}

// WithTLSConfig selects TLS or mutual TLS using a defensive clone of config.
func WithTLSConfig(config *tls.Config) Option {
	return func(client *clientConfig) error {
		if config == nil {
			return errors.New("yesnodb TLS config must not be nil")
		}
		client.credentials = credentials.NewTLS(config.Clone())
		return nil
	}
}

// WithGRPCDialOptions appends transport options such as keepalive parameters.
// Credentials must still be selected explicitly with WithInsecureTransport or
// WithTLSConfig.
func WithGRPCDialOptions(options ...grpc.DialOption) Option {
	return func(config *clientConfig) error {
		config.grpcOptions = append(config.grpcOptions, options...)
		return nil
	}
}

// Client is a yesnodb-shaped convenience layer over an Arrow Flight client.
//
// Client methods may be called concurrently. Close must run only after those
// calls and their QueryStreams have finished.
type Client struct {
	flight    flight.Client
	allocator memory.Allocator
	token     TokenProvider
	term      atomic.Uint32
}

// Dial constructs an owned Arrow Flight connection.
//
// Transport security is explicit: callers must provide WithInsecureTransport
// for plaintext or WithTLSConfig for TLS.
func Dial(target string, options ...Option) (*Client, error) {
	config, err := applyOptions(options)
	if err != nil {
		return nil, err
	}
	if config.credentials == nil {
		return nil, errors.New("yesnodb Dial requires WithInsecureTransport or WithTLSConfig")
	}
	dialOptions := []grpc.DialOption{grpc.WithTransportCredentials(config.credentials)}
	dialOptions = append(dialOptions, config.grpcOptions...)
	connection, err := grpc.NewClient(target, dialOptions...)
	if err != nil {
		return nil, err
	}
	client := newConfiguredClient(flight.NewClientFromConn(connection, nil), config)
	return client, nil
}

// NewClient wraps and takes ownership of an already configured Flight client.
// The caller retains ownership of any allocator passed with WithAllocator.
func NewClient(inner flight.Client, options ...Option) (*Client, error) {
	if inner == nil {
		return nil, errors.New("yesnodb Flight client must not be nil")
	}
	config, err := applyOptions(options)
	if err != nil {
		return nil, err
	}
	return newConfiguredClient(inner, config), nil
}

func applyOptions(options []Option) (clientConfig, error) {
	config := defaultClientConfig()
	for _, option := range options {
		if option == nil {
			return clientConfig{}, errors.New("yesnodb client option must not be nil")
		}
		if err := option(&config); err != nil {
			return clientConfig{}, err
		}
	}
	return config, nil
}

func newConfiguredClient(inner flight.Client, config clientConfig) *Client {
	client := &Client{flight: inner, allocator: config.allocator, token: config.token}
	client.term.Store(config.minimumTerm)
	return client
}

// FlightClient exposes the underlying Arrow Flight client for protocol features
// that are not yesnodb-specific.
func (c *Client) FlightClient() flight.Client { return c.flight }

// MinimumTerm returns the monotonic leadership floor learned by this client.
func (c *Client) MinimumTerm() uint32 { return c.term.Load() }

// Close closes the owned Flight connection.
func (c *Client) Close() error { return c.flight.Close() }

func (c *Client) rpcContext(ctx context.Context) (context.Context, error) {
	if ctx == nil {
		return nil, errors.New("yesnodb RPC context must not be nil")
	}
	if c.token != nil {
		token, err := c.token(ctx)
		if err != nil {
			return nil, fmt.Errorf("yesnodb token provider: %w", err)
		}
		if token == "" || strings.ContainsAny(token, "\r\n") {
			return nil, errors.New("yesnodb token provider returned an empty or multiline token")
		}
		ctx = metadata.AppendToOutgoingContext(ctx, "authorization", "Bearer "+token)
	}
	if term := c.term.Load(); term != 0 {
		ctx = metadata.AppendToOutgoingContext(ctx, headerExpectTerm, strconv.FormatUint(uint64(term), 10))
	}
	return ctx, nil
}

func (c *Client) observeHeaders(headers metadata.MD) error {
	for _, raw := range headers.Get(headerTerm) {
		term, err := strconv.ParseUint(strings.TrimSpace(raw), 10, 32)
		if err != nil {
			return protocolErrorf("metadata", "server returned invalid %s value %q", headerTerm, raw)
		}
		for {
			current := c.term.Load()
			if uint32(term) <= current || c.term.CompareAndSwap(current, uint32(term)) {
				break
			}
		}
	}
	return nil
}

// QueryInfo is the exact metadata and versioned ticket produced by planning.
type QueryInfo struct {
	totalRecords uint64
	ticket       Ticket
	ticketBytes  []byte
	flightInfo   *flight.FlightInfo
}

// TotalRecords returns the exact result cardinality.
func (q *QueryInfo) TotalRecords() uint64 { return q.totalRecords }

// Version returns the database version shared by the count and ticket.
func (q *QueryInfo) Version() uint64 { return q.ticket.Version }

// Ticket returns the validated decoded ticket.
func (q *QueryInfo) Ticket() Ticket { return q.ticket }

// TicketBytes returns a defensive copy of the opaque Flight ticket.
func (q *QueryInfo) TicketBytes() []byte { return append([]byte(nil), q.ticketBytes...) }

// FlightInfo returns a defensive clone of the complete Arrow Flight metadata.
func (q *QueryInfo) FlightInfo() *flight.FlightInfo {
	return proto.Clone(q.flightInfo).(*flight.FlightInfo)
}

func (c *Client) queryInfo(info *flight.FlightInfo) (*QueryInfo, error) {
	if info == nil {
		return nil, protocolError("planning", "server returned no FlightInfo")
	}
	if info.TotalRecords < 0 {
		return nil, protocolErrorf("planning", "server returned negative total_records %d", info.TotalRecords)
	}
	if len(info.Endpoint) != 1 {
		return nil, protocolErrorf("planning", "server returned %d endpoints instead of 1", len(info.Endpoint))
	}
	endpoint := info.Endpoint[0]
	if endpoint == nil || endpoint.Ticket == nil {
		return nil, protocolError("planning", "server returned no query ticket")
	}
	ticketBytes := append([]byte(nil), endpoint.Ticket.Ticket...)
	ticket, err := DecodeTicket(ticketBytes)
	if err != nil {
		return nil, protocolErrorf("planning", "server returned a malformed query ticket: %v", err)
	}
	schema, err := flight.DeserializeSchema(info.Schema, c.allocator)
	if err != nil {
		return nil, protocolErrorf("planning", "server returned a malformed Arrow schema: %v", err)
	}
	if !isOrdinalSchema(schema) {
		return nil, protocolErrorf("planning", "server returned the wrong query schema: %s", schema)
	}
	return &QueryInfo{
		totalRecords: uint64(info.TotalRecords),
		ticket:       ticket,
		ticketBytes:  ticketBytes,
		flightInfo:   proto.Clone(info).(*flight.FlightInfo),
	}, nil
}

func isOrdinalSchema(schema *arrow.Schema) bool {
	if schema == nil || len(schema.Fields()) != 1 {
		return false
	}
	field := schema.Field(0)
	return field.Name == "ordinal" && !field.Nullable && arrow.TypeEqual(field.Type, arrow.PrimitiveTypes.Uint64)
}

func commandDescriptor(command []byte) *flight.FlightDescriptor {
	return &flight.FlightDescriptor{Type: flight.DescriptorCMD, Cmd: append([]byte(nil), command...)}
}

// Keys returns every populated key in server order.
func (c *Client) Keys(ctx context.Context) ([]uint64, error) {
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return nil, err
	}
	stream, err := c.flight.ListFlights(ctx, &flight.Criteria{})
	if err != nil {
		return nil, err
	}
	if headers, headerErr := stream.Header(); headerErr == nil {
		if err := c.observeHeaders(headers); err != nil {
			return nil, err
		}
	}
	var keys []uint64
	for {
		info, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return keys, nil
		}
		if err != nil {
			return nil, err
		}
		if info.FlightDescriptor == nil || len(info.FlightDescriptor.Cmd) != 8 {
			return nil, protocolError("keys", "server returned a descriptor other than one 8-byte key")
		}
		keys = append(keys, binary.LittleEndian.Uint64(info.FlightDescriptor.Cmd))
	}
}

// PrepareKey plans one key without fetching ordinals.
func (c *Client) PrepareKey(ctx context.Context, key uint64) (*QueryInfo, error) {
	command := make([]byte, 8)
	binary.LittleEndian.PutUint64(command, key)
	return c.PrepareCommand(ctx, command)
}

// PrepareQuery plans one expression at the current database version.
func (c *Client) PrepareQuery(ctx context.Context, expression Expr) (*QueryInfo, error) {
	command, err := EncodeExpression(expression)
	if err != nil {
		return nil, err
	}
	return c.PrepareCommand(ctx, command)
}

// PrepareQueryAt strictly plans one expression at an exact database version.
func (c *Client) PrepareQueryAt(ctx context.Context, expression Expr, version uint64) (*QueryInfo, error) {
	command, err := QueryAt(expression, version).Encode()
	if err != nil {
		return nil, err
	}
	return c.PrepareCommand(ctx, command)
}

// PrepareCommand plans an already encoded yesnodb descriptor command.
func (c *Client) PrepareCommand(ctx context.Context, command []byte) (*QueryInfo, error) {
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return nil, err
	}
	var headers metadata.MD
	info, err := c.flight.GetFlightInfo(ctx, commandDescriptor(command), grpc.Header(&headers))
	if err != nil {
		return nil, err
	}
	if err := c.observeHeaders(headers); err != nil {
		return nil, err
	}
	return c.queryInfo(info)
}

// Cardinality returns one key's exact count without fetching ordinals.
func (c *Client) Cardinality(ctx context.Context, key uint64) (uint64, error) {
	info, err := c.PrepareKey(ctx, key)
	if err != nil {
		return 0, err
	}
	return info.TotalRecords(), nil
}

// QueryCardinality returns an expression's exact count without fetching ordinals.
func (c *Client) QueryCardinality(ctx context.Context, expression Expr) (uint64, error) {
	info, err := c.PrepareQuery(ctx, expression)
	if err != nil {
		return 0, err
	}
	return info.TotalRecords(), nil
}

// Fetch opens a validated stream for a previously planned query.
func (c *Client) Fetch(ctx context.Context, info *QueryInfo) (*QueryStream, error) {
	if info == nil {
		return nil, errors.New("yesnodb query info must not be nil")
	}
	reader, err := c.fetchTicket(ctx, info.ticketBytes)
	if err != nil {
		return nil, err
	}
	return &QueryStream{info: info, reader: reader}, nil
}

// FetchTicket opens a raw Arrow reader for an opaque ticket. Use Fetch when
// exact count validation is required.
func (c *Client) FetchTicket(ctx context.Context, ticket []byte) (*flight.Reader, error) {
	return c.fetchTicket(ctx, ticket)
}

func (c *Client) fetchTicket(ctx context.Context, ticket []byte) (*flight.Reader, error) {
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return nil, err
	}
	stream, err := c.flight.DoGet(ctx, &flight.Ticket{Ticket: append([]byte(nil), ticket...)})
	if err != nil {
		return nil, err
	}
	if headers, headerErr := stream.Header(); headerErr == nil {
		if err := c.observeHeaders(headers); err != nil {
			return nil, err
		}
	}
	reader, err := flight.NewRecordReader(stream, ipc.WithAllocator(c.allocator))
	if err != nil {
		return nil, err
	}
	return reader, nil
}

// Get plans and fetches one key.
func (c *Client) Get(ctx context.Context, key uint64) (*QueryStream, error) {
	info, err := c.PrepareKey(ctx, key)
	if err != nil {
		return nil, err
	}
	return c.Fetch(ctx, info)
}

// Query plans and fetches one expression.
func (c *Client) Query(ctx context.Context, expression Expr) (*QueryStream, error) {
	info, err := c.PrepareQuery(ctx, expression)
	if err != nil {
		return nil, err
	}
	return c.Fetch(ctx, info)
}

// Insert sends pairs in bounded 8,192-row batches. Each batch is one commit.
func (c *Client) Insert(ctx context.Context, pairs []Pair) (uint64, error) {
	return c.putPairs(ctx, pairs, putInsert, batchRows)
}

// Remove sends pairs in bounded 8,192-row batches. Each batch is one commit.
func (c *Client) Remove(ctx context.Context, pairs []Pair) (uint64, error) {
	return c.putPairs(ctx, pairs, putRemove, batchRows)
}

// InsertBatch sends all pairs in one Arrow batch and one commit.
func (c *Client) InsertBatch(ctx context.Context, pairs []Pair) (uint64, error) {
	return c.putPairs(ctx, pairs, putInsert, max(1, len(pairs)))
}

// RemoveBatch sends all pairs in one Arrow batch and one commit.
func (c *Client) RemoveBatch(ctx context.Context, pairs []Pair) (uint64, error) {
	return c.putPairs(ctx, pairs, putRemove, max(1, len(pairs)))
}

// InsertBatchAcked is InsertBatch, reporting the commit version alongside the
// count.
//
// This is the read-your-writes primitive: pass Ack.Version to PrepareQueryAt and
// the read is bound to a database state containing the write. One batch is one
// commit, so the version names exactly this call's write -- unlike the streaming
// forms, which commit per batch and report the last version.
func (c *Client) InsertBatchAcked(ctx context.Context, pairs []Pair) (Ack, error) {
	return c.putPairsAcked(ctx, pairs, putInsert, max(1, len(pairs)))
}

// RemoveBatchAcked is RemoveBatch, reporting the commit version alongside the count.
func (c *Client) RemoveBatchAcked(ctx context.Context, pairs []Pair) (Ack, error) {
	return c.putPairsAcked(ctx, pairs, putRemove, max(1, len(pairs)))
}

// InsertRecords sends caller-owned records, preserving one commit per record.
func (c *Client) InsertRecords(ctx context.Context, records []arrow.RecordBatch) (uint64, error) {
	return c.putRecords(ctx, records, putInsert)
}

// RemoveRecords sends caller-owned records, preserving one commit per record.
func (c *Client) RemoveRecords(ctx context.Context, records []arrow.RecordBatch) (uint64, error) {
	return c.putRecords(ctx, records, putRemove)
}

func (c *Client) putPairs(ctx context.Context, pairs []Pair, command string, rows int) (uint64, error) {
	for _, pair := range pairs {
		if err := pair.Validate(); err != nil {
			return 0, err
		}
	}
	records := make([]arrow.RecordBatch, 0, (len(pairs)+rows-1)/rows)
	defer func() {
		for _, record := range records {
			record.Release()
		}
	}()
	for start := 0; start < len(pairs); start += rows {
		end := min(start+rows, len(pairs))
		records = append(records, c.pairRecord(pairs[start:end]))
	}
	return c.putRecords(ctx, records, command)
}

func (c *Client) putPairsAcked(ctx context.Context, pairs []Pair, command string, rows int) (Ack, error) {
	for _, pair := range pairs {
		if err := pair.Validate(); err != nil {
			return Ack{}, err
		}
	}
	records := make([]arrow.RecordBatch, 0, (len(pairs)+rows-1)/rows)
	defer func() {
		for _, record := range records {
			record.Release()
		}
	}()
	for start := 0; start < len(pairs); start += rows {
		end := min(start+rows, len(pairs))
		records = append(records, c.pairRecord(pairs[start:end]))
	}
	return c.putRecordsAcked(ctx, records, command)
}

func (c *Client) pairRecord(pairs []Pair) arrow.RecordBatch {
	keys := array.NewUint64Builder(c.allocator)
	ordinals := array.NewUint64Builder(c.allocator)
	defer keys.Release()
	defer ordinals.Release()
	keys.Reserve(len(pairs))
	ordinals.Reserve(len(pairs))
	for _, pair := range pairs {
		keys.Append(pair.Key)
		ordinals.Append(pair.Ordinal)
	}
	keyArray := keys.NewArray()
	ordinalArray := ordinals.NewArray()
	defer keyArray.Release()
	defer ordinalArray.Release()
	return array.NewRecordBatch(PairsSchema(), []arrow.Array{keyArray, ordinalArray}, int64(len(pairs)))
}

func (c *Client) putRecords(ctx context.Context, records []arrow.RecordBatch, command string) (uint64, error) {
	ack, err := c.putRecordsAcked(ctx, records, command)
	return ack.Rows, err
}

func (c *Client) putRecordsAcked(ctx context.Context, records []arrow.RecordBatch, command string) (Ack, error) {
	var sent uint64
	for _, record := range records {
		if err := validatePairsRecord(record); err != nil {
			return Ack{}, err
		}
		rows := uint64(record.NumRows())
		if sent > math.MaxUint64-rows {
			return Ack{}, errors.New("yesnodb pair count overflows uint64")
		}
		sent += rows
	}
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return Ack{}, err
	}
	stream, err := c.flight.DoPut(ctx)
	if err != nil {
		return Ack{}, err
	}
	writer := flight.NewRecordWriter(stream, ipc.WithSchema(PairsSchema()), ipc.WithAllocator(c.allocator))
	writer.SetFlightDescriptor(commandDescriptor([]byte(command)))
	for _, record := range records {
		if err := writer.Write(record); err != nil {
			return Ack{}, err
		}
	}
	if err := writer.Close(); err != nil {
		return Ack{}, err
	}
	if err := stream.CloseSend(); err != nil {
		return Ack{}, err
	}
	if headers, headerErr := stream.Header(); headerErr == nil {
		if err := c.observeHeaders(headers); err != nil {
			return Ack{}, err
		}
	}
	var acknowledged *Ack
	for {
		result, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return Ack{}, err
		}
		ack, err := decodeAck(result.AppMetadata)
		if err != nil {
			return Ack{}, err
		}
		acknowledged = &ack
	}
	if acknowledged == nil {
		return Ack{}, protocolError("ingest", "server returned no acknowledgement")
	}
	if acknowledged.Rows != sent {
		return Ack{}, protocolErrorf("ingest", "server acknowledged %d of %d pairs", acknowledged.Rows, sent)
	}
	return *acknowledged, nil
}

// Ack is what the server acknowledged for one ingest call.
//
// Version is optional because of wire compatibility: DoPut's app_metadata
// carried 8 bytes -- the row count alone -- until 2026-09-12, when the commit
// version was appended. Decoding accepts both widths and reports a zero Version
// for the short form, so a new client against an old server loses the version
// rather than the call. A Version of 0 never means version zero: that is the
// empty database and no commit is assigned it, so a server that committed
// nothing reports 0 as well.
type Ack struct {
	// Rows is the number of pairs the server accepted and committed.
	Rows uint64
	// Version is the database version at which those rows are present, or 0
	// when the server reported none.
	Version uint64
}

func decodeAck(metadata []byte) (Ack, error) {
	if len(metadata) != 8 && len(metadata) != 16 {
		return Ack{}, protocolErrorf("ingest", "acknowledgement has %d bytes, expected 8 or 16", len(metadata))
	}
	ack := Ack{Rows: binary.LittleEndian.Uint64(metadata[:8])}
	if len(metadata) == 16 {
		ack.Version = binary.LittleEndian.Uint64(metadata[8:16])
	}
	return ack, nil
}

// WriteTxn names an open write transaction.
type WriteTxn uint64

// Supports reports whether the server implements every bit in features.
//
// Ask before calling Apply or BeginWrite. A server predating the capability
// field reports zero and treats an unrecognised DoPut command as insert, so an
// unchecked mixed batch would have its removals applied as insertions with no
// error anywhere.
func (c *Client) Supports(ctx context.Context, features uint64) (bool, error) {
	stats, err := c.Stats(ctx)
	if err != nil {
		return false, err
	}
	return stats.Features&features == features, nil
}

// Apply applies mixed operations as one commit.
//
// Every record batch accumulates into a single write batch that commits once
// at end of stream, so the returned version is the single instant at which
// every row became visible. Operations apply in the order given, which is what
// lets a change-data feed replay a delete-then-reinsert of one key without the
// two reordering. This is the difference from Insert, which commits per record
// batch and can only report the last of several versions.
func (c *Client) Apply(ctx context.Context, mutations []Mutation) (Ack, error) {
	return c.putMutations(ctx, "apply", mutations)
}

// BeginWrite opens a write transaction spanning several calls.
func (c *Client) BeginWrite(ctx context.Context) (WriteTxn, error) {
	handle, err := c.uint64Action(ctx, "begin_write", nil)
	return WriteTxn(handle), err
}

// Stage stages mutations into an open transaction, returning the rows accepted.
//
// Staged rows are invisible to readers until CommitWrite.
//
// A Stage error is fatal to the whole transaction. Mutations are sent in
// batches, and a failure in a later batch leaves earlier ones staged with no
// way to withdraw them, so the server poisons the transaction: CommitWrite
// will refuse it and AbortWrite is the only way out. Do not retry a failed
// Stage in place.
func (c *Client) Stage(ctx context.Context, txn WriteTxn, mutations []Mutation) (uint64, error) {
	command := make([]byte, 0, 12)
	command = append(command, "txn:"...)
	command = binary.LittleEndian.AppendUint64(command, uint64(txn))
	ack, err := c.putMutations(ctx, string(command), mutations)
	return ack.Rows, err
}

// CommitWrite publishes everything staged in txn and returns its version.
//
// Idempotent within one live service, and only there. The server remembers a
// bounded number of recent outcomes in memory, so a prompt retry after a lost
// response returns the original version rather than committing twice. A
// restart loses every outcome and later traffic evicts older ones, so this
// does not cover the case a change-data pipe has to survive: losing the
// response and then finding the server restarted. Such a caller cannot learn
// the version its transaction committed at. Handles do not recur, so replaying
// the old one fails closed rather than resolving a different transaction, but
// failing closed is not recovering.
func (c *Client) CommitWrite(ctx context.Context, txn WriteTxn) (uint64, error) {
	return c.uint64Action(ctx, "commit_write", binary.LittleEndian.AppendUint64(nil, uint64(txn)))
}

// AbortWrite discards everything staged in txn.
//
// Aborting a transaction the server no longer knows about succeeds: it was
// already aborted or it expired, and either way the caller's intent -- that
// none of it is visible -- already holds. Aborting one that committed is an
// error, because a version exists that says otherwise.
func (c *Client) AbortWrite(ctx context.Context, txn WriteTxn) error {
	_, err := c.uint64Action(ctx, "abort_write", binary.LittleEndian.AppendUint64(nil, uint64(txn)))
	return err
}

func (c *Client) mutationRecord(mutations []Mutation) arrow.RecordBatch {
	builder := array.NewRecordBuilder(c.allocator, MutationsSchema())
	defer builder.Release()
	keys := builder.Field(0).(*array.Uint64Builder)
	lo := builder.Field(1).(*array.Uint64Builder)
	hi := builder.Field(2).(*array.Uint64Builder)
	op := builder.Field(3).(*array.Uint8Builder)
	keys.Reserve(len(mutations))
	lo.Reserve(len(mutations))
	hi.Reserve(len(mutations))
	op.Reserve(len(mutations))
	for _, m := range mutations {
		keys.Append(m.Key)
		lo.Append(m.Lo)
		hi.Append(m.Hi)
		op.Append(m.Op)
	}
	return builder.NewRecordBatch()
}

func (c *Client) putMutations(ctx context.Context, command string, mutations []Mutation) (Ack, error) {
	for _, m := range mutations {
		if err := m.Validate(); err != nil {
			return Ack{}, err
		}
	}
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return Ack{}, err
	}
	stream, err := c.flight.DoPut(ctx)
	if err != nil {
		return Ack{}, err
	}
	writer := flight.NewRecordWriter(stream, ipc.WithSchema(MutationsSchema()), ipc.WithAllocator(c.allocator))
	writer.SetFlightDescriptor(commandDescriptor([]byte(command)))
	sent := uint64(0)
	for start := 0; start < len(mutations); start += batchRows {
		end := start + batchRows
		if end > len(mutations) {
			end = len(mutations)
		}
		record := c.mutationRecord(mutations[start:end])
		err := writer.Write(record)
		record.Release()
		if err != nil {
			return Ack{}, err
		}
		sent += uint64(end - start)
	}
	if err := writer.Close(); err != nil {
		return Ack{}, err
	}
	if err := stream.CloseSend(); err != nil {
		return Ack{}, err
	}
	if headers, headerErr := stream.Header(); headerErr == nil {
		if err := c.observeHeaders(headers); err != nil {
			return Ack{}, err
		}
	}
	var acknowledged *Ack
	for {
		result, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return Ack{}, err
		}
		ack, err := decodeAck(result.AppMetadata)
		if err != nil {
			return Ack{}, err
		}
		acknowledged = &ack
	}
	if acknowledged == nil {
		return Ack{}, protocolError("ingest", "server returned no acknowledgement")
	}
	if acknowledged.Rows != sent {
		return Ack{}, protocolErrorf("ingest", "server acknowledged %d of %d mutations", acknowledged.Rows, sent)
	}
	return *acknowledged, nil
}

// Stats returns typed server space and reader counters.
func (c *Client) Stats(ctx context.Context) (ServerStats, error) {
	body, err := c.action(ctx, "stats", nil)
	if err != nil {
		return ServerStats{}, err
	}
	stats, err := DecodeServerStats(body)
	if err != nil {
		return ServerStats{}, protocolErrorf("stats", "server returned malformed protobuf: %v", err)
	}
	return stats, nil
}

// Clear atomically removes a whole key and returns the commit version.
func (c *Client) Clear(ctx context.Context, key uint64) (uint64, error) {
	return c.uint64Action(ctx, "clear", binary.LittleEndian.AppendUint64(nil, key))
}

// Contains tests one pair without fetching its posting list.
func (c *Client) Contains(ctx context.Context, key, ordinal uint64) (bool, error) {
	return c.booleanPairAction(ctx, "contains", key, ordinal)
}

// InsertOne atomically inserts one pair and reports whether the set changed.
func (c *Client) InsertOne(ctx context.Context, key, ordinal uint64) (bool, error) {
	if err := (Pair{Key: key, Ordinal: ordinal}).Validate(); err != nil {
		return false, err
	}
	return c.booleanPairAction(ctx, "insert_one", key, ordinal)
}

// RemoveOne atomically removes one pair and reports whether the set changed.
func (c *Client) RemoveOne(ctx context.Context, key, ordinal uint64) (bool, error) {
	if err := (Pair{Key: key, Ordinal: ordinal}).Validate(); err != nil {
		return false, err
	}
	return c.booleanPairAction(ctx, "remove_one", key, ordinal)
}

func (c *Client) booleanPairAction(ctx context.Context, name string, key, ordinal uint64) (bool, error) {
	body := binary.LittleEndian.AppendUint64(nil, key)
	body = binary.LittleEndian.AppendUint64(body, ordinal)
	value, err := c.uint64Action(ctx, name, body)
	if err != nil {
		return false, err
	}
	switch value {
	case 0:
		return false, nil
	case 1:
		return true, nil
	default:
		return false, protocolErrorf(name, "server returned %d instead of zero or one", value)
	}
}

func (c *Client) uint64Action(ctx context.Context, name string, request []byte) (uint64, error) {
	body, err := c.action(ctx, name, request)
	if err != nil {
		return 0, err
	}
	if len(body) != 8 {
		return 0, protocolErrorf(name, "server returned %d bytes instead of 8", len(body))
	}
	return binary.LittleEndian.Uint64(body), nil
}

func (c *Client) action(ctx context.Context, name string, request []byte) ([]byte, error) {
	ctx, err := c.rpcContext(ctx)
	if err != nil {
		return nil, err
	}
	stream, err := c.flight.DoAction(ctx, &flight.Action{Type: name, Body: append([]byte(nil), request...)})
	if err != nil {
		return nil, err
	}
	if headers, headerErr := stream.Header(); headerErr == nil {
		if err := c.observeHeaders(headers); err != nil {
			return nil, err
		}
	}
	var body []byte
	for {
		result, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return body, nil
		}
		if err != nil {
			return nil, err
		}
		body = append(body, result.Body...)
	}
}
