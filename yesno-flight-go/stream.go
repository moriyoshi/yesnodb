package yesnodb

import (
	"math"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/flight"
)

// A peer controls FlightInfo.TotalRecords. Avoid trusting an otherwise valid
// but implausibly large advertised count as an eager allocation request.
const collectPreallocateLimit = 1 << 20

// QueryStream incrementally validates and exposes Arrow result batches.
//
// Record is owned by the underlying reader and remains valid until the next
// call to Next. Call Retain before keeping it longer. Release must be called
// when the stream is no longer needed.
type QueryStream struct {
	info        *QueryInfo
	reader      *flight.Reader
	received    uint64
	previous    uint64
	hasPrevious bool
	err         error
	finished    bool
	released    bool
}

// Info returns the exact query metadata associated with this stream.
func (s *QueryStream) Info() *QueryInfo { return s.info }

// Next advances to and validates another Arrow record batch.
func (s *QueryStream) Next() bool {
	if s.finished || s.released {
		return false
	}
	if !s.reader.Next() {
		s.finished = true
		if err := s.reader.Err(); err != nil {
			s.err = err
		} else if s.received != s.info.TotalRecords() {
			s.err = protocolErrorf("query", "server returned %d ordinals after promising %d", s.received, s.info.TotalRecords())
		}
		return false
	}
	values, err := validateOrdinalRecord(s.reader.RecordBatch())
	if err != nil {
		s.err = err
		s.finished = true
		return false
	}
	rows := uint64(values.Len())
	if s.received > math.MaxUint64-rows {
		s.err = protocolError("query", "received ordinal count overflowed uint64")
		s.finished = true
		return false
	}
	for index := 0; index < values.Len(); index++ {
		value := values.Value(index)
		if s.hasPrevious && value <= s.previous {
			s.err = protocolError("query", "server returned ordinals outside strict ascending set order")
			s.finished = true
			return false
		}
		s.previous = value
		s.hasPrevious = true
	}
	s.received += rows
	if s.received > s.info.TotalRecords() {
		s.err = protocolErrorf("query", "server exceeded promised cardinality %d", s.info.TotalRecords())
		s.finished = true
		return false
	}
	return true
}

// Record returns the current Arrow batch. It is nil before the first successful
// Next call and after the stream finishes.
func (s *QueryStream) Record() arrow.RecordBatch {
	if s.released || s.reader == nil {
		return nil
	}
	return s.reader.RecordBatch()
}

// Err reports the first transport or protocol error encountered by Next.
func (s *QueryStream) Err() error { return s.err }

// CollectOrdinals materializes all remaining ordinals.
func (s *QueryStream) CollectOrdinals() ([]uint64, error) {
	capacity := 0
	maxInt := uint64(^uint(0) >> 1)
	if promised := s.info.TotalRecords(); promised <= maxInt && promised <= collectPreallocateLimit {
		capacity = int(promised)
	}
	ordinals := make([]uint64, 0, capacity)
	for s.Next() {
		values, err := validateOrdinalRecord(s.Record())
		if err != nil {
			return nil, err
		}
		for index := 0; index < values.Len(); index++ {
			ordinals = append(ordinals, values.Value(index))
		}
	}
	if err := s.Err(); err != nil {
		return nil, err
	}
	return ordinals, nil
}

// Release cancels local consumption and releases Arrow buffers.
func (s *QueryStream) Release() {
	if s.released {
		return
	}
	s.released = true
	s.finished = true
	if s.reader != nil {
		s.reader.Release()
		s.reader = nil
	}
}
